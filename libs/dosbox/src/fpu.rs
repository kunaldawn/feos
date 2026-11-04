//! The x87 floating-point unit.
//!
//! The eight 80-bit registers come from `rust-dos`'s `F80` type (originally
//! based on mwemu); see LICENSING.md. This module adds the x87 register stack,
//! the control and status words and the D8-DF instruction encodings. The
//! arithmetic itself, all of it in 80 bits, is in [`crate::f80`].
//!
//! Exceptions are reported but never delivered. Every instruction produces the
//! x87's masked response, whatever the control word says: it sets the exception
//! flags, and an unmasked one also sets the ES and B summary bits, but there is
//! no FERR, no interrupt 10h or 75h, and the destination is written as if the
//! exception had been masked.
//!
//! The FOP field of a saved environment leaves the mod bits of a memory
//! operand's modrm byte clear, because the CPU's decoded operand doesn't keep
//! them.
//!
//! The saved instruction pointer is the address after the instruction rather
//! than its start, because the CPU doesn't expose where the instruction began.

use core::cmp::Ordering;

use crate::cpu::{Bus, Cpu, Exec, ModRm, AX, FLAG_AF, FLAG_CF, FLAG_OF, FLAG_PF, FLAG_SF, FLAG_ZF};
use crate::f80::{self, Class, Constant, Env, BCD_INDEFINITE, DE, F80, IE, RC_CHOP, UE, ZE};

const TAG_EMPTY: u16 = 3;

const DEFAULT_CONTROL: u16 = 0x037F;

/// The control word's writable bits. Bit 6 always reads as one, as on a 387.
const CONTROL_MASK: u16 = 0x1F3F;
const CONTROL_ONES: u16 = 0x0040;

const STATUS_SF: u16 = 0x0040;
const STATUS_ES: u16 = 0x0080;
const STATUS_C0: u16 = 0x0100;
const STATUS_C1: u16 = 0x0200;
const STATUS_C2: u16 = 0x0400;
const STATUS_TOP: u16 = 0x3800;
const STATUS_C3: u16 = 0x4000;
const STATUS_B: u16 = 0x8000;
const STATUS_CC: u16 = STATUS_C0 | STATUS_C1 | STATUS_C2 | STATUS_C3;

/// The size in bytes of the eight registers in an FNSAVE image.
const SAVE_REGISTERS: u32 = 80;

/// A comparison's result: `None` is unordered.
type Compared = Option<Ordering>;

/// The x87 state.
#[derive(Clone)]
pub struct Fpu {
    /// Physical registers, indexed by `top + i`.
    pub st: [F80; 8],
    pub top: u8,
    pub control: u16,
    /// The status word, without TOP; [`Fpu::status_word`] adds it.
    pub status: u16,
    /// Two tag bits per physical register.
    pub tags: u16,
    /// The last instruction's offset or linear address.
    pub ip: u32,
    pub cs: u16,
    /// The last instruction's opcode: 11 bits.
    pub opcode: u16,
    /// The last memory operand's offset or linear address.
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
            st: [F80::new(); 8],
            top: 0,
            control: DEFAULT_CONTROL,
            status: 0,
            tags: 0xFFFF,
            ip: 0,
            cs: 0,
            opcode: 0,
            data: 0,
            ds: 0,
        }
    }

    /// FNINIT. The register contents are kept; they just become empty.
    pub fn init(&mut self) {
        self.top = 0;
        self.control = DEFAULT_CONTROL;
        self.status = 0;
        self.tags = 0xFFFF;
        self.ip = 0;
        self.cs = 0;
        self.opcode = 0;
        self.data = 0;
        self.ds = 0;
    }

    /// The status word as the guest reads it. ES and B follow the exception
    /// flags and masks, so a changed mask is seen straight away.
    pub fn status_word(&self) -> u16 {
        let mut word = self.status & !(STATUS_TOP | STATUS_ES | STATUS_B);
        word |= ((self.top & 7) as u16) << 11;
        // A stack fault is reported through IE, so SF has no mask of its own.
        if self.status & 0x3F & !self.control != 0 {
            word |= STATUS_ES | STATUS_B;
        }
        word
    }

    fn physical(&self, index: u8) -> usize {
        (self.top.wrapping_add(index) & 7) as usize
    }

    fn set_tag_physical(&mut self, physical: usize, tag: u16) {
        let shift = (physical & 7) * 2;
        self.tags = (self.tags & !(3 << shift)) | ((tag & 3) << shift);
    }

    fn tag_physical(&self, physical: usize) -> u16 {
        (self.tags >> ((physical & 7) * 2)) & 3
    }

    fn is_empty(&self, index: u8) -> bool {
        self.tag_physical(self.physical(index)) == TAG_EMPTY
    }

    /// Reads a register, whether or not it's empty.
    pub fn get(&self, index: u8) -> F80 {
        self.st[self.physical(index)]
    }

    /// Writes a register and sets its tag from the value.
    pub fn set(&mut self, index: u8, value: F80) {
        let physical = self.physical(index);
        self.st[physical] = value;
        self.set_tag_physical(physical, value.tag());
    }

    /// Pushes without checking for overflow.
    fn push_raw(&mut self, value: F80) {
        self.top = self.top.wrapping_sub(1) & 7;
        self.set(0, value);
    }

    /// Pushes, with the masked response to overflow: the indefinite.
    fn push(&mut self, value: F80) {
        if self.is_empty(7) {
            self.push_raw(value);
        } else {
            self.stack_overflow();
            self.push_raw(F80::INDEFINITE);
        }
    }

    fn pop(&mut self) {
        let physical = self.physical(0);
        self.set_tag_physical(physical, TAG_EMPTY);
        self.top = self.top.wrapping_add(1) & 7;
    }

    fn stack_underflow(&mut self) {
        self.status |= IE | STATUS_SF;
        self.status &= !STATUS_C1;
    }

    fn stack_overflow(&mut self) {
        self.status |= IE | STATUS_SF | STATUS_C1;
    }

    fn clear_c1(&mut self) {
        self.status &= !STATUS_C1;
    }

    fn rc(&self) -> u8 {
        ((self.control >> 10) & 3) as u8
    }

    /// An environment for operations that ignore precision control.
    fn env(&self) -> Env {
        let mut env = Env::new(self.rc());
        env.underflow_unmasked = self.control & UE == 0;
        env
    }

    /// An environment for the operations that honour precision control: add,
    /// subtract, multiply, divide and square root.
    fn env_precise(&self) -> Env {
        let mut env = self.env();
        env.prec = match (self.control >> 8) & 3 {
            0 => 24,
            2 => 53,
            _ => 64,
        };
        env
    }

    /// Takes an operation's exception flags and its rounded-up bit.
    fn commit(&mut self, env: &Env) {
        self.status &= !STATUS_C1;
        self.status |= env.flags & (0x3F | STATUS_C1);
    }

    fn set_condition(&mut self, compared: Compared) {
        self.status &= !(STATUS_C0 | STATUS_C2 | STATUS_C3);
        self.status |= match compared {
            Some(Ordering::Greater) => 0,
            Some(Ordering::Less) => STATUS_C0,
            Some(Ordering::Equal) => STATUS_C3,
            None => STATUS_C0 | STATUS_C2 | STATUS_C3,
        };
    }

    /// Compares two values. `quiet` is the FUCOM family, for which only a
    /// signalling NaN is invalid.
    fn compare_values(&mut self, a: F80, b: F80, quiet: bool) -> Compared {
        let (class_a, class_b) = (a.class(), b.class());
        let unsupported = class_a == Class::Unsupported || class_b == Class::Unsupported;
        let signalling = class_a == Class::SNan || class_b == Class::SNan;
        if unsupported || signalling {
            self.status |= IE;
            return None;
        }
        if a.is_nan() || b.is_nan() {
            if !quiet {
                self.status |= IE;
            }
            return None;
        }
        if class_a == Class::Denormal || class_b == Class::Denormal {
            self.status |= DE;
        }
        Some(f80::compare(a, b))
    }

    /// Compares ST(0) with a value.
    fn compare_top(&mut self, other: F80, quiet: bool) -> Compared {
        self.clear_c1();
        if self.is_empty(0) {
            self.stack_underflow();
            return None;
        }
        self.compare_values(self.get(0), other, quiet)
    }

    /// Compares ST(0) with ST(i).
    fn compare_register(&mut self, index: u8, quiet: bool) -> Compared {
        self.clear_c1();
        if self.is_empty(0) || self.is_empty(index) {
            self.stack_underflow();
            return None;
        }
        self.compare_values(self.get(0), self.get(index), quiet)
    }

    /// One arithmetic operation, numbered like the `reg` field of the D8 table:
    /// 0 add, 1 multiply, 4 `a - b`, 5 `b - a`, 6 `a / b`, 7 `b / a`.
    fn arithmetic(&mut self, op: u8, a: F80, b: F80, denormal: bool) -> F80 {
        let mut env = self.env_precise();
        let result = match op {
            0 => f80::add(&mut env, a, b),
            1 => f80::mul(&mut env, a, b),
            4 => f80::sub(&mut env, a, b),
            5 => f80::sub(&mut env, b, a),
            6 => f80::div(&mut env, a, b),
            _ => f80::div(&mut env, b, a),
        };
        // A denormal memory operand was normalized on the way in, so it's
        // reported here, unless something outranks it.
        if denormal && env.flags & (IE | ZE) == 0 && !a.is_nan() {
            env.flags |= DE;
        }
        self.commit(&env);
        result
    }

    /// `ST(dst) = ST(dst) op ST(src)`, with the masked response to an empty
    /// register.
    fn arithmetic_registers(&mut self, op: u8, dst: u8, src: u8) {
        if self.is_empty(dst) || self.is_empty(src) {
            self.stack_underflow();
            self.set(dst, F80::INDEFINITE);
            return;
        }
        let result = self.arithmetic(op, self.get(dst), self.get(src), false);
        self.set(dst, result);
    }

    /// `ST(0) = ST(0) op value`, or a comparison, for a memory operand.
    fn arithmetic_memory(&mut self, op: u8, value: F80, denormal: bool) {
        match op {
            2 | 3 => {
                let compared = self.compare_top(value, false);
                if denormal && compared.is_some() {
                    self.status |= DE;
                }
                self.set_condition(compared);
                if op == 3 {
                    self.pop();
                }
            }
            _ => {
                if self.is_empty(0) {
                    self.stack_underflow();
                    self.set(0, F80::INDEFINITE);
                    return;
                }
                let result = self.arithmetic(op, self.get(0), value, denormal);
                self.set(0, result);
            }
        }
    }

    /// Runs a one-operand function on ST(0).
    fn unary(&mut self, function: fn(&mut Env, F80) -> F80) {
        if self.is_empty(0) {
            self.stack_underflow();
            self.set(0, F80::INDEFINITE);
            return;
        }
        let mut env = self.env();
        let result = function(&mut env, self.get(0));
        self.commit(&env);
        self.set(0, result);
    }

    /// Runs a function of ST(1) and ST(0), leaving its result in ST(1), then pops.
    fn binary_pop(&mut self, function: fn(&mut Env, F80, F80) -> F80, st1_first: bool) {
        if self.is_empty(0) || self.is_empty(1) {
            self.stack_underflow();
            self.set(1, F80::INDEFINITE);
        } else {
            let mut env = self.env();
            let (a, b) = (self.get(0), self.get(1));
            let result = if st1_first {
                function(&mut env, b, a)
            } else {
                function(&mut env, a, b)
            };
            self.commit(&env);
            self.set(1, result);
        }
        self.pop();
    }

    /// FSIN and FCOS: their checks on ST(0). True means the instruction is over:
    /// empty, or out of range with C2 set.
    fn trig_refused(&mut self) -> bool {
        self.status &= !STATUS_C2;
        if self.is_empty(0) {
            self.stack_underflow();
            self.set(0, F80::INDEFINITE);
            return true;
        }
        self.out_of_range()
    }

    /// Sets C2 and clears C1 if ST(0) is too big for a trigonometric function.
    fn out_of_range(&mut self) -> bool {
        if f80::trig_out_of_range(self.get(0)) {
            self.clear_c1();
            self.status |= STATUS_C2;
            return true;
        }
        false
    }

    /// For the instructions that replace ST(0) and push. A full stack, or an
    /// empty ST(0), leaves the indefinite in both places.
    fn replace_and_push_refused(&mut self) -> bool {
        if self.is_empty(0) {
            self.stack_underflow();
        } else if !self.is_empty(7) {
            self.stack_overflow();
        } else {
            return false;
        }
        self.set(0, F80::INDEFINITE);
        self.push_raw(F80::INDEFINITE);
        true
    }

    fn fptan(&mut self) {
        self.status &= !STATUS_C2;
        if self.replace_and_push_refused() || self.out_of_range() {
            return;
        }
        let mut env = self.env();
        let value = self.get(0);
        let result = f80::tan(&mut env, value);
        self.commit(&env);
        self.set(0, result);
        // An invalid operand gives the indefinite twice over.
        if result.is_nan() {
            self.push_raw(result);
        } else {
            self.push_raw(f80::from_i64(1));
        }
    }

    fn fsincos(&mut self) {
        self.status &= !STATUS_C2;
        if self.replace_and_push_refused() || self.out_of_range() {
            return;
        }
        let mut env = self.env();
        let value = self.get(0);
        let sine = f80::sin(&mut env, value);
        let cosine = f80::cos(&mut env, value);
        self.commit(&env);
        self.set(0, sine);
        self.push_raw(cosine);
    }

    fn fxtract(&mut self) {
        if self.replace_and_push_refused() {
            return;
        }
        let mut env = self.env();
        let (exponent, significand) = f80::extract(&mut env, self.get(0));
        self.commit(&env);
        self.set(0, exponent);
        self.push_raw(significand);
    }

    fn fprem(&mut self, nearest: bool) {
        self.status &= !(STATUS_C0 | STATUS_C2 | STATUS_C3);
        if self.is_empty(0) || self.is_empty(1) {
            self.stack_underflow();
            self.set(0, F80::INDEFINITE);
            return;
        }
        let mut env = self.env();
        let result = f80::remainder(&mut env, self.get(0), self.get(1), nearest);
        self.commit(&env);
        self.set(0, result.value);
        self.clear_c1();
        if result.partial {
            self.status |= STATUS_C2;
        }
        if result.quotient & 1 != 0 {
            self.status |= STATUS_C1;
        }
        if result.quotient & 2 != 0 {
            self.status |= STATUS_C3;
        }
        if result.quotient & 4 != 0 {
            self.status |= STATUS_C0;
        }
    }

    fn fscale(&mut self) {
        if self.is_empty(0) || self.is_empty(1) {
            self.stack_underflow();
            self.set(0, F80::INDEFINITE);
            return;
        }
        let mut env = self.env();
        let result = f80::scale(&mut env, self.get(0), self.get(1));
        self.commit(&env);
        self.set(0, result);
    }

    fn fxch(&mut self, index: u8) {
        self.clear_c1();
        if self.is_empty(0) || self.is_empty(index) {
            self.stack_underflow();
            if self.is_empty(0) {
                self.set(0, F80::INDEFINITE);
            }
            if self.is_empty(index) {
                self.set(index, F80::INDEFINITE);
            }
        }
        let (a, b) = (self.get(0), self.get(index));
        self.set(0, b);
        self.set(index, a);
    }

    fn fld_register(&mut self, index: u8) {
        self.clear_c1();
        if self.is_empty(index) {
            self.stack_underflow();
            self.push(F80::INDEFINITE);
        } else {
            let value = self.get(index);
            self.push(value);
        }
    }

    /// FST and FSTP to a register.
    fn fst_register(&mut self, index: u8, pop: bool) {
        self.clear_c1();
        if self.is_empty(0) {
            self.stack_underflow();
            self.set(index, F80::INDEFINITE);
        } else {
            let value = self.get(0);
            self.set(index, value);
        }
        if pop {
            self.pop();
        }
    }

    /// FXAM's condition codes.
    fn fxam(&mut self) {
        let value = self.get(0);
        self.status &= !STATUS_CC;
        if value.sign() {
            self.status |= STATUS_C1;
        }
        self.status |= if self.is_empty(0) {
            STATUS_C3 | STATUS_C0
        } else {
            match value.class() {
                Class::Unsupported => 0,
                Class::QNan | Class::SNan => STATUS_C0,
                Class::Normal => STATUS_C2,
                Class::Infinity => STATUS_C2 | STATUS_C0,
                Class::Zero => STATUS_C3,
                Class::Denormal => STATUS_C3 | STATUS_C2,
            }
        };
    }

    fn constant(&mut self, which: Constant) {
        self.clear_c1();
        let value = f80::constant(self.rc(), which);
        self.push(value);
    }

    fn load_one_or_zero(&mut self, value: i64) {
        self.clear_c1();
        self.push(f80::from_i64(value));
    }

    /// The tag word, worked out afresh from the registers and which are empty.
    pub fn tag_word(&self) -> u16 {
        let mut word = 0;
        for physical in 0..8 {
            let tag = if self.tag_physical(physical) == TAG_EMPTY {
                TAG_EMPTY
            } else {
                self.st[physical].tag()
            };
            word |= tag << (physical * 2);
        }
        word
    }

    fn load_tag_word(&mut self, word: u16) {
        self.tags = word;
        self.tags = self.tag_word();
    }

    fn load_control(&mut self, word: u16) {
        self.control = (word & CONTROL_MASK) | CONTROL_ONES;
    }

    fn load_status(&mut self, word: u16) {
        self.top = ((word >> 11) & 7) as u8;
        self.status = word & !(STATUS_TOP | STATUS_ES | STATUS_B);
    }

    /// An environment image: 14 bytes for a 16-bit operand, 28 for a 32-bit one.
    /// Real mode holds linear addresses, protected mode offsets and selectors.
    fn environment(&self, wide: bool, protected: bool) -> ([u8; 28], u32) {
        let mut image = [0u8; 28];
        let (control, status, tags) = (self.control, self.status_word(), self.tag_word());
        let opcode = self.opcode & 0x7FF;
        let (ip, data) = (self.ip, self.data);
        fn put(image: &mut [u8; 28], at: usize, value: u32) {
            if let Some(bytes) = image.get_mut(at..at + 4) {
                bytes.copy_from_slice(&value.to_le_bytes());
            }
        }
        fn put16(image: &mut [u8; 28], at: usize, value: u16) {
            if let Some(bytes) = image.get_mut(at..at + 2) {
                bytes.copy_from_slice(&value.to_le_bytes());
            }
        }
        if wide {
            const HIGH: u32 = 0xFFFF_0000;
            put(&mut image, 0, HIGH | control as u32);
            put(&mut image, 4, HIGH | status as u32);
            put(&mut image, 8, HIGH | tags as u32);
            if protected {
                put(&mut image, 12, ip);
                put(&mut image, 16, self.cs as u32 | ((opcode as u32) << 16));
                put(&mut image, 20, data);
                put(&mut image, 24, HIGH | self.ds as u32);
            } else {
                put(&mut image, 12, HIGH | (ip & 0xFFFF));
                put(&mut image, 16, ((ip >> 16) << 12) | opcode as u32);
                put(&mut image, 20, HIGH | (data & 0xFFFF));
                put(&mut image, 24, (data >> 16) << 12);
            }
            (image, 28)
        } else {
            put16(&mut image, 0, control);
            put16(&mut image, 2, status);
            put16(&mut image, 4, tags);
            put16(&mut image, 6, ip as u16);
            put16(&mut image, 10, data as u16);
            if protected {
                put16(&mut image, 8, self.cs);
                put16(&mut image, 12, self.ds);
            } else {
                put16(&mut image, 8, (((ip >> 16) << 12) as u16) | opcode);
                put16(&mut image, 12, ((data >> 16) << 12) as u16);
            }
            (image, 14)
        }
    }

    /// Loads an environment image written by [`Fpu::environment`] or a guest.
    fn load_environment(&mut self, image: &[u8; 28], wide: bool, protected: bool) {
        let get16 = |at: usize| u16::from_le_bytes([image[at], image[at + 1]]);
        let get32 = |at: usize| {
            u32::from_le_bytes([image[at], image[at + 1], image[at + 2], image[at + 3]])
        };
        if wide {
            self.load_control(get16(0));
            self.load_status(get16(4));
            self.load_tag_word(get16(8));
            if protected {
                self.ip = get32(12);
                self.cs = get16(16);
                self.opcode = get16(18) & 0x7FF;
                self.data = get32(20);
                self.ds = get16(24);
            } else {
                let (word, data_high) = (get32(16), get32(24));
                self.ip = (get16(12) as u32) | ((word >> 12) << 16);
                self.opcode = word as u16 & 0x7FF;
                self.data = (get16(20) as u32) | ((data_high >> 12) << 16);
                self.cs = 0;
                self.ds = 0;
            }
        } else {
            self.load_control(get16(0));
            self.load_status(get16(2));
            self.load_tag_word(get16(4));
            if protected {
                self.ip = get16(6) as u32;
                self.cs = get16(8);
                self.data = get16(10) as u32;
                self.ds = get16(12);
            } else {
                self.ip = (get16(6) as u32) | (((get16(8) >> 12) as u32) << 16);
                self.opcode = get16(8) & 0x7FF;
                self.data = (get16(10) as u32) | (((get16(12) >> 12) as u32) << 16);
                self.cs = 0;
                self.ds = 0;
            }
        }
    }

    /// The D9 register forms, other than what needs the CPU.
    fn d9_register(&mut self, modrm: u8) {
        let index = modrm & 7;
        match modrm {
            0xC0..=0xC7 => self.fld_register(index),
            0xC8..=0xCF => self.fxch(index),
            // FSTP1, an undocumented alias.
            0xD8..=0xDF => self.fst_register(index, true),
            0xE0 | 0xE1 => {
                self.clear_c1();
                if self.is_empty(0) {
                    self.stack_underflow();
                    self.set(0, F80::INDEFINITE);
                } else {
                    let value = self.get(0);
                    let sign = if modrm == 0xE0 { !value.sign() } else { false };
                    self.set(0, value.with_sign(sign));
                }
            }
            0xE4 => {
                let compared = self.compare_top(F80::new(), false);
                self.set_condition(compared);
            }
            0xE5 => self.fxam(),
            0xE8 => self.load_one_or_zero(1),
            0xE9 => self.constant(Constant::Log2Of10),
            0xEA => self.constant(Constant::Log2OfE),
            0xEB => self.constant(Constant::Pi),
            0xEC => self.constant(Constant::Log10Of2),
            0xED => self.constant(Constant::LnOf2),
            0xEE => self.load_one_or_zero(0),
            0xF0 => self.unary(f80::f2xm1),
            0xF1 => self.binary_pop(f80::yl2x, false),
            0xF2 => self.fptan(),
            0xF3 => self.binary_pop(f80::atan2, true),
            0xF4 => self.fxtract(),
            0xF5 => self.fprem(true),
            0xF6 => {
                self.clear_c1();
                self.top = self.top.wrapping_sub(1) & 7;
            }
            0xF7 => {
                self.clear_c1();
                self.top = self.top.wrapping_add(1) & 7;
            }
            0xF8 => self.fprem(false),
            0xF9 => self.binary_pop(f80::yl2xp1, false),
            0xFA => {
                if self.is_empty(0) {
                    self.stack_underflow();
                    self.set(0, F80::INDEFINITE);
                } else {
                    let mut env = self.env_precise();
                    let result = f80::sqrt(&mut env, self.get(0));
                    self.commit(&env);
                    self.set(0, result);
                }
            }
            0xFB => self.fsincos(),
            0xFC => self.unary(f80::round_to_integer),
            0xFD => self.fscale(),
            0xFE => {
                if !self.trig_refused() {
                    self.unary(f80::sin);
                }
            }
            0xFF => {
                if !self.trig_refused() {
                    self.unary(f80::cos);
                }
            }
            // The remaining encodings do nothing.
            _ => {}
        }
    }
}

impl Cpu {
    /// Runs one D8-DF instruction, after the CPU's device-not-available checks
    /// and modrm decode. A memory operand faults before any FPU state changes,
    /// so the instruction can restart. Undefined encodings do nothing.
    pub(crate) fn fpu_execute<B: Bus>(&mut self, bus: &mut B, opcode: u8, m: &ModRm) -> Exec {
        if m.is_reg {
            self.fpu_register(opcode, m);
            Ok(())
        } else {
            self.fpu_memory(bus, opcode, m)
        }
    }

    /// Records the pointers a saved environment reports. Only the operations
    /// that can raise a numeric exception update them.
    fn fpu_note(&mut self, opcode: u8, m: &ModRm) {
        let protected = self.protected_mode();
        let cs = self.sregs[crate::cpu::CS];
        let start = self.instr_start();
        let fpu_ip = if protected {
            start
        } else {
            ((cs as u32) << 4).wrapping_add(start)
        };
        let modrm = if m.is_reg { 0xC0 } else { 0 } | (m.reg << 3) | m.rm;
        self.fpu.ip = fpu_ip;
        self.fpu.cs = if protected { cs } else { 0 };
        self.fpu.opcode = (((opcode & 7) as u16) << 8) | modrm as u16;
        if !m.is_reg {
            let selector = self.sregs.get(m.seg as usize).copied().unwrap_or(0);
            if protected {
                self.fpu.data = m.off;
                self.fpu.ds = selector;
            } else {
                self.fpu.data = ((selector as u32) << 4).wrapping_add(m.off);
                self.fpu.ds = 0;
            }
        }
    }

    /// Sets ZF, PF and CF like FCOMI, and clears OF, SF and AF.
    fn fpu_set_eflags(&mut self, compared: Compared) {
        let (zf, pf, cf) = match compared {
            Some(Ordering::Greater) => (false, false, false),
            Some(Ordering::Less) => (false, false, true),
            Some(Ordering::Equal) => (true, false, false),
            None => (true, true, true),
        };
        self.set_flag(FLAG_ZF, zf);
        self.set_flag(FLAG_PF, pf);
        self.set_flag(FLAG_CF, cf);
        self.set_flag(FLAG_OF, false);
        self.set_flag(FLAG_SF, false);
        self.set_flag(FLAG_AF, false);
    }

    fn fpu_fcmov(&mut self, index: u8, condition: bool) {
        self.fpu.clear_c1();
        if self.fpu.is_empty(0) || self.fpu.is_empty(index) {
            self.fpu.stack_underflow();
            if condition {
                self.fpu.set(0, F80::INDEFINITE);
            }
        } else if condition {
            let value = self.fpu.get(index);
            self.fpu.set(0, value);
        }
    }

    fn fpu_register(&mut self, opcode: u8, m: &ModRm) {
        let modrm = 0xC0 | (m.reg << 3) | m.rm;
        let index = m.rm;
        let control = matches!((opcode, modrm), (0xDB, 0xE0..=0xE4) | (0xDF, 0xE0));
        if !control {
            self.fpu_note(opcode, m);
        }
        match opcode {
            0xD8 => match m.reg {
                2 | 3 => {
                    let compared = self.fpu.compare_register(index, false);
                    self.fpu.set_condition(compared);
                    if m.reg == 3 {
                        self.fpu.pop();
                    }
                }
                op => self.fpu.arithmetic_registers(op, 0, index),
            },
            0xD9 => self.fpu.d9_register(modrm),
            0xDA => match modrm {
                0xC0..=0xC7 => self.fpu_fcmov(index, self.cf()),
                0xC8..=0xCF => self.fpu_fcmov(index, self.zf()),
                0xD0..=0xD7 => self.fpu_fcmov(index, self.cf() || self.zf()),
                0xD8..=0xDF => self.fpu_fcmov(index, self.pf()),
                0xE9 => {
                    let compared = self.fpu.compare_register(1, true);
                    self.fpu.set_condition(compared);
                    self.fpu.pop();
                    self.fpu.pop();
                }
                _ => {}
            },
            0xDB => match modrm {
                0xC0..=0xC7 => self.fpu_fcmov(index, !self.cf()),
                0xC8..=0xCF => self.fpu_fcmov(index, !self.zf()),
                0xD0..=0xD7 => self.fpu_fcmov(index, !(self.cf() || self.zf())),
                0xD8..=0xDF => self.fpu_fcmov(index, !self.pf()),
                // FNENI, FNDISI and FSETPM are 8087 and 287 leftovers.
                0xE0 | 0xE1 | 0xE4 => {}
                0xE2 => self.fpu.status &= !(0x3F | STATUS_SF | STATUS_ES | STATUS_B),
                0xE3 => self.fpu.init(),
                0xE8..=0xEF => {
                    let compared = self.fpu.compare_register(index, true);
                    self.fpu_set_eflags(compared);
                }
                0xF0..=0xF7 => {
                    let compared = self.fpu.compare_register(index, false);
                    self.fpu_set_eflags(compared);
                }
                _ => {}
            },
            // FCOM2, FCOMP3 and FCOMP5 are undocumented aliases of FCOM and FCOMP.
            0xDC | 0xDE if m.reg == 2 || (m.reg == 3 && opcode == 0xDC) => {
                let compared = self.fpu.compare_register(index, false);
                self.fpu.set_condition(compared);
                if m.reg == 3 || opcode == 0xDE {
                    self.fpu.pop();
                }
            }
            0xDE if m.reg == 3 => {
                if modrm == 0xD9 {
                    let compared = self.fpu.compare_register(1, false);
                    self.fpu.set_condition(compared);
                    self.fpu.pop();
                    self.fpu.pop();
                }
            }
            // Subtract and divide swap senses against D8's numbering: DC E0+i is
            // FSUBR ST(i), ST, which is ST(i) = ST - ST(i), DC E8+i is FSUB
            // ST(i), ST, F0+i is FDIVR and F8+i is FDIV. Seen from ST(i) as the
            // destination, that's exactly D8's reversed operation.
            0xDC | 0xDE => {
                let op = if m.reg >= 4 { m.reg ^ 1 } else { m.reg };
                self.fpu.arithmetic_registers(op, index, 0);
                if opcode == 0xDE {
                    self.fpu.pop();
                }
            }
            0xDD => match m.reg {
                0 => {
                    let physical = self.fpu.physical(index);
                    self.fpu.set_tag_physical(physical, TAG_EMPTY);
                }
                1 => self.fpu.fxch(index),
                2 => self.fpu.fst_register(index, false),
                3 => self.fpu.fst_register(index, true),
                4 | 5 => {
                    let compared = self.fpu.compare_register(index, true);
                    self.fpu.set_condition(compared);
                    if m.reg == 5 {
                        self.fpu.pop();
                    }
                }
                _ => {}
            },
            _ => match modrm {
                // FFREEP, undocumented but widely used.
                0xC0..=0xC7 => {
                    let physical = self.fpu.physical(index);
                    self.fpu.set_tag_physical(physical, TAG_EMPTY);
                    self.fpu.pop();
                }
                0xC8..=0xCF => self.fpu.fxch(index),
                0xD0..=0xDF => self.fpu.fst_register(index, true),
                0xE0 => {
                    let word = self.fpu.status_word();
                    self.set_reg16(AX as u8, word);
                }
                0xE8..=0xEF => {
                    let compared = self.fpu.compare_register(index, true);
                    self.fpu_set_eflags(compared);
                    self.fpu.pop();
                }
                0xF0..=0xF7 => {
                    let compared = self.fpu.compare_register(index, false);
                    self.fpu_set_eflags(compared);
                    self.fpu.pop();
                }
                _ => {}
            },
        }
    }

    fn fpu_read32<B: Bus>(&mut self, bus: &mut B, m: &ModRm, at: u32) -> Exec<u32> {
        let off = self.wrap_addr(m.off.wrapping_add(at));
        self.rd32(bus, m.seg as usize, off)
    }

    fn fpu_read64<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> Exec<u64> {
        let low = self.fpu_read32(bus, m, 0)?;
        let high = self.fpu_read32(bus, m, 4)?;
        Ok(low as u64 | ((high as u64) << 32))
    }

    fn fpu_read_bytes<B: Bus>(&mut self, bus: &mut B, m: &ModRm, out: &mut [u8]) -> Exec {
        for (index, byte) in out.iter_mut().enumerate() {
            let off = self.wrap_addr(m.off.wrapping_add(index as u32));
            *byte = self.rd8(bus, m.seg as usize, off)?;
        }
        Ok(())
    }

    /// Writes bytes. The last goes first: a limit or page fault on the far end
    /// then happens before anything has been written, and FPU state is only
    /// changed after this returns, so the instruction restarts cleanly.
    fn fpu_write_bytes<B: Bus>(&mut self, bus: &mut B, m: &ModRm, bytes: &[u8]) -> Exec {
        let seg = m.seg as usize;
        if let Some(last) = bytes.last() {
            let at = bytes.len().wrapping_sub(1) as u32;
            let off = self.wrap_addr(m.off.wrapping_add(at));
            self.wr8(bus, seg, off, *last)?;
        }
        for (index, byte) in bytes.iter().enumerate() {
            let off = self.wrap_addr(m.off.wrapping_add(index as u32));
            self.wr8(bus, seg, off, *byte)?;
        }
        Ok(())
    }

    /// FST and FSTP to a single or double.
    fn fpu_store_real<B: Bus>(&mut self, bus: &mut B, m: &ModRm, double: bool, pop: bool) -> Exec {
        let empty = self.fpu.is_empty(0);
        let value = if empty {
            F80::INDEFINITE
        } else {
            self.fpu.get(0)
        };
        let mut env = self.fpu.env();
        let bits = if double {
            f80::to_ieee(&mut env, value, 52, 11)
        } else {
            f80::to_ieee(&mut env, value, 23, 8)
        };
        if double {
            self.fpu_write_bytes(bus, m, &bits.to_le_bytes())?;
        } else {
            self.fpu_write_bytes(bus, m, &(bits as u32).to_le_bytes())?;
        }
        self.fpu.commit(&env);
        if empty {
            self.fpu.stack_underflow();
        }
        if pop {
            self.fpu.pop();
        }
        Ok(())
    }

    /// FIST and FISTP: `bytes` is the integer's size. `chop` is FISTTP.
    fn fpu_store_integer<B: Bus>(
        &mut self,
        bus: &mut B,
        m: &ModRm,
        bytes: u32,
        pop: bool,
        chop: bool,
    ) -> Exec {
        let empty = self.fpu.is_empty(0);
        let mut env = self.fpu.env();
        if chop {
            env.rc = RC_CHOP;
        }
        let bits = bytes.wrapping_mul(8);
        let integer = if empty {
            None
        } else {
            f80::to_integer(&mut env, self.fpu.get(0), bits)
        };
        // The integer indefinite is the most negative value.
        let value = integer.unwrap_or(i64::MIN >> (64 - bits.clamp(1, 64)));
        let image = value.to_le_bytes();
        self.fpu_write_bytes(bus, m, image.get(..bytes as usize).unwrap_or(&image))?;
        self.fpu.commit(&env);
        if empty {
            self.fpu.stack_underflow();
        }
        if pop {
            self.fpu.pop();
        }
        Ok(())
    }

    fn fpu_fbstp<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> Exec {
        let empty = self.fpu.is_empty(0);
        let mut env = self.fpu.env();
        let bytes = if empty {
            None
        } else {
            f80::to_bcd(&mut env, self.fpu.get(0))
        };
        self.fpu_write_bytes(bus, m, &bytes.unwrap_or(BCD_INDEFINITE))?;
        self.fpu.commit(&env);
        if empty {
            self.fpu.stack_underflow();
        }
        self.fpu.pop();
        Ok(())
    }

    /// FST(P) to an 80-bit operand copies the pattern as it stands.
    fn fpu_fstp80<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> Exec {
        let empty = self.fpu.is_empty(0);
        let value = if empty {
            F80::INDEFINITE
        } else {
            self.fpu.get(0)
        };
        self.fpu_write_bytes(bus, m, &value.get_bytes())?;
        self.fpu.clear_c1();
        if empty {
            self.fpu.stack_underflow();
        }
        self.fpu.pop();
        Ok(())
    }

    /// Loads a single or double, which is exact. Only a signalling NaN, which
    /// quietens, and a denormal, which normalizes, raise anything.
    fn fpu_load_real(&mut self, value: F80, denormal: bool) {
        self.fpu.clear_c1();
        let mut value = value;
        if value.class() == Class::SNan {
            self.fpu.status |= IE;
            value = F80 {
                st: value.st | (1u128 << 62),
            };
        }
        if denormal && self.fpu.is_empty(7) {
            self.fpu.status |= DE;
        }
        self.fpu.push(value);
    }

    fn fpu_stenv<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> Exec {
        let wide = self.vw() == 32;
        let protected = self.protected_mode();
        let (image, size) = self.fpu.environment(wide, protected);
        let bytes = image.get(..size as usize).unwrap_or(&image);
        self.fpu_write_bytes(bus, m, bytes)?;
        // FNSTENV also masks every exception.
        self.fpu.control |= 0x3F;
        Ok(())
    }

    fn fpu_ldenv<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> Exec {
        let wide = self.vw() == 32;
        let protected = self.protected_mode();
        let mut image = [0u8; 28];
        let size = if wide { 28 } else { 14 };
        self.fpu_read_bytes(bus, m, &mut image[..size])?;
        self.fpu.load_environment(&image, wide, protected);
        Ok(())
    }

    fn fpu_fnsave<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> Exec {
        let wide = self.vw() == 32;
        let protected = self.protected_mode();
        let (image, size) = self.fpu.environment(wide, protected);
        let total = size.wrapping_add(SAVE_REGISTERS) as usize;
        let mut buffer = [0u8; 108];
        let header = image.get(..size as usize).unwrap_or(&image);
        buffer[..header.len()].copy_from_slice(header);
        for index in 0..8u8 {
            let at = size as usize + index as usize * 10;
            let bytes = self.fpu.get(index).get_bytes();
            if let Some(slot) = buffer.get_mut(at..at + 10) {
                slot.copy_from_slice(&bytes);
            }
        }
        self.fpu_write_bytes(bus, m, buffer.get(..total).unwrap_or(&buffer))?;
        self.fpu.init();
        Ok(())
    }

    fn fpu_frstor<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> Exec {
        let wide = self.vw() == 32;
        let protected = self.protected_mode();
        let size = if wide { 28 } else { 14 };
        let mut buffer = [0u8; 108];
        let total = size + SAVE_REGISTERS as usize;
        self.fpu_read_bytes(bus, m, &mut buffer[..total])?;
        let mut image = [0u8; 28];
        image[..size].copy_from_slice(&buffer[..size]);
        // TOP has to be in place before the registers go in, and the tags after.
        self.fpu.load_environment(&image, wide, protected);
        let tags = if wide {
            u16::from_le_bytes([image[8], image[9]])
        } else {
            u16::from_le_bytes([image[4], image[5]])
        };
        for index in 0..8usize {
            let at = size + index * 10;
            let mut bytes = [0u8; 10];
            if let Some(slot) = buffer.get(at..at + 10) {
                bytes.copy_from_slice(slot);
            }
            let physical = self.fpu.physical(index as u8);
            self.fpu.st[physical].set_bytes(&bytes);
        }
        self.fpu.load_tag_word(tags);
        Ok(())
    }

    fn fpu_memory<B: Bus>(&mut self, bus: &mut B, opcode: u8, m: &ModRm) -> Exec {
        let seg = m.seg as usize;
        match (opcode, m.reg) {
            // The control instructions, which leave the pointers alone.
            (0xD9, 4) => return self.fpu_ldenv(bus, m),
            (0xD9, 5) => {
                let word = self.rd16(bus, seg, m.off)?;
                self.fpu.load_control(word);
                return Ok(());
            }
            (0xD9, 6) => return self.fpu_stenv(bus, m),
            (0xD9, 7) => {
                let word = self.fpu.control;
                return self.wr16(bus, seg, m.off, word);
            }
            (0xDD, 4) => return self.fpu_frstor(bus, m),
            (0xDD, 6) => return self.fpu_fnsave(bus, m),
            (0xDD, 7) => {
                let word = self.fpu.status_word();
                return self.wr16(bus, seg, m.off, word);
            }
            // The undefined memory forms.
            (0xD9, 1) | (0xDB, 4) | (0xDB, 6) | (0xDD, 5) => return Ok(()),
            _ => {}
        }
        match (opcode, m.reg) {
            (0xD8, op) => {
                let bits = self.rd32(bus, seg, m.off)? as u64;
                let (value, denormal) = f80::from_ieee(bits, 23, 8);
                self.fpu.arithmetic_memory(op, value, denormal);
            }
            (0xDC, op) => {
                let bits = self.fpu_read64(bus, m)?;
                let (value, denormal) = f80::from_ieee(bits, 52, 11);
                self.fpu.arithmetic_memory(op, value, denormal);
            }
            (0xDA, op) => {
                let value = self.rd32(bus, seg, m.off)? as i32 as i64;
                self.fpu.arithmetic_memory(op, f80::from_i64(value), false);
            }
            (0xDE, op) => {
                let value = self.rd16(bus, seg, m.off)? as i16 as i64;
                self.fpu.arithmetic_memory(op, f80::from_i64(value), false);
            }
            (0xD9, 0) => {
                let bits = self.rd32(bus, seg, m.off)? as u64;
                let (value, denormal) = f80::from_ieee(bits, 23, 8);
                self.fpu_load_real(value, denormal);
            }
            (0xD9, 2) => self.fpu_store_real(bus, m, false, false)?,
            (0xD9, _) => self.fpu_store_real(bus, m, false, true)?,
            (0xDB, 0) => {
                let value = self.rd32(bus, seg, m.off)? as i32 as i64;
                self.fpu.clear_c1();
                self.fpu.push(f80::from_i64(value));
            }
            (0xDB, 1) => self.fpu_store_integer(bus, m, 4, true, true)?,
            (0xDB, 2) => self.fpu_store_integer(bus, m, 4, false, false)?,
            (0xDB, 3) => self.fpu_store_integer(bus, m, 4, true, false)?,
            (0xDB, 5) => {
                let mut bytes = [0u8; 10];
                self.fpu_read_bytes(bus, m, &mut bytes)?;
                let mut value = F80::new();
                value.set_bytes(&bytes);
                self.fpu.clear_c1();
                self.fpu.push(value);
            }
            (0xDB, _) => self.fpu_fstp80(bus, m)?,
            (0xDD, 0) => {
                let bits = self.fpu_read64(bus, m)?;
                let (value, denormal) = f80::from_ieee(bits, 52, 11);
                self.fpu_load_real(value, denormal);
            }
            (0xDD, 1) => self.fpu_store_integer(bus, m, 8, true, true)?,
            (0xDD, 2) => self.fpu_store_real(bus, m, true, false)?,
            (0xDD, _) => self.fpu_store_real(bus, m, true, true)?,
            (_, 0) => {
                let value = self.rd16(bus, seg, m.off)? as i16 as i64;
                self.fpu.clear_c1();
                self.fpu.push(f80::from_i64(value));
            }
            (_, 1) => self.fpu_store_integer(bus, m, 2, true, true)?,
            (_, 2) => self.fpu_store_integer(bus, m, 2, false, false)?,
            (_, 3) => self.fpu_store_integer(bus, m, 2, true, false)?,
            (_, 4) => {
                let mut bytes = [0u8; 10];
                self.fpu_read_bytes(bus, m, &mut bytes)?;
                self.fpu.clear_c1();
                self.fpu.push(f80::from_bcd(&bytes));
            }
            (_, 5) => {
                let value = self.fpu_read64(bus, m)? as i64;
                self.fpu.clear_c1();
                self.fpu.push(f80::from_i64(value));
            }
            (_, 6) => self.fpu_fbstp(bus, m)?,
            (_, _) => self.fpu_store_integer(bus, m, 8, true, false)?,
        }
        self.fpu_note(opcode, m);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::{Seg, Step, CS, DS};
    use crate::f80::from_i64;
    use alloc::vec::Vec;

    /// Where a table row's memory operand lives, in segment 0.
    const OPERAND: usize = 0x0200;
    const CODE: usize = 0x1_0000;
    /// Control words: everything masked, full precision, per rounding mode.
    const NEAREST: u16 = 0x037F;
    const DOWN: u16 = 0x077F;
    const UP: u16 = 0x0B7F;
    const CHOP: u16 = 0x0F7F;

    const SW_IE: u16 = 0x0001;
    const SW_DE: u16 = 0x0002;
    const SW_ZE: u16 = 0x0004;
    const SW_OE: u16 = 0x0008;
    const SW_UE: u16 = 0x0010;
    const SW_PE: u16 = 0x0020;
    const SW_SF: u16 = 0x0040;
    const SW_ES: u16 = 0x0080;
    const C0: u16 = 0x0100;
    const C1: u16 = 0x0200;
    const C2: u16 = 0x0400;
    const C3: u16 = 0x4000;
    const SW_B: u16 = 0x8000;
    /// Everything but TOP, for rows that give the exact status.
    const NOT_TOP: u16 = 0xC7FF;
    const FLAGS_ONLY: u16 = 0x00FF;
    const CC: u16 = C0 | C1 | C2 | C3;

    const POS_INF: u128 = 0x7FFF_8000_0000_0000_0000;
    const NEG_INF: u128 = 0xFFFF_8000_0000_0000_0000;
    const POS_ZERO: u128 = 0;
    const NEG_ZERO: u128 = 0x8000_0000_0000_0000_0000;
    const QNAN: u128 = 0x7FFF_C000_0000_0000_0001;
    const SNAN: u128 = 0x7FFF_8000_0000_0000_0001;
    const INDEFINITE: u128 = 0xFFFF_C000_0000_0000_0000;
    const UNNORMAL: u128 = 0x4000_4000_0000_0000_0000;
    const DENORMAL: u128 = 0x0000_0000_0000_0000_0001;
    const SMALLEST_NORMAL: u128 = 0x0001_8000_0000_0000_0000;
    const LARGEST: u128 = 0x7FFE_FFFF_FFFF_FFFF_FFFF;
    const HALF: u128 = 0x3FFE_8000_0000_0000_0000;
    const ONE_POINT_FIVE: u128 = 0x3FFF_C000_0000_0000_0000;
    const TWO_POINT_FIVE: u128 = 0x4000_A000_0000_0000_0000;
    const MINUS_2_5: u128 = 0xC000_A000_0000_0000_0000;
    const TWO_63: u128 = 0x403E_8000_0000_0000_0000;
    const TWO_70: u128 = 0x4045_8000_0000_0000_0000;
    /// One plus the last bit of the significand.
    const ONE_PLUS: u128 = 0x3FFF_8000_0000_0000_0001;

    struct Mem {
        data: Vec<u8>,
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

    fn i(value: i64) -> u128 {
        from_i64(value).st
    }

    /// One table row: a machine code sequence with the state it starts from and
    /// what it must leave behind.
    #[derive(Clone)]
    struct Case {
        name: &'static str,
        code: Vec<u8>,
        control: u16,
        /// ST(0) first.
        stack: Vec<u128>,
        memory: Vec<u8>,
        want_stack: Option<Vec<u128>>,
        want_memory: Vec<u8>,
        status_mask: u16,
        status: u16,
        eflags: Option<u32>,
        ulps: u64,
    }

    fn case(name: &'static str, code: &[u8]) -> Case {
        Case {
            name,
            code: code.to_vec(),
            control: NEAREST,
            stack: Vec::new(),
            memory: Vec::new(),
            want_stack: None,
            want_memory: Vec::new(),
            status_mask: 0,
            status: 0,
            eflags: None,
            ulps: 0,
        }
    }

    impl Case {
        fn cw(mut self, control: u16) -> Self {
            self.control = control;
            self
        }
        fn st(mut self, stack: &[u128]) -> Self {
            self.stack = stack.to_vec();
            self
        }
        fn mem(mut self, memory: &[u8]) -> Self {
            self.memory = memory.to_vec();
            self
        }
        fn want(mut self, stack: &[u128]) -> Self {
            self.want_stack = Some(stack.to_vec());
            self
        }
        fn out(mut self, memory: &[u8]) -> Self {
            self.want_memory = memory.to_vec();
            self
        }
        /// The status bits under `mask` must equal `value`.
        fn sw(mut self, mask: u16, value: u16) -> Self {
            self.status_mask = mask;
            self.status = value;
            self
        }
        /// Exactly these exception flags.
        fn ex(self, value: u16) -> Self {
            self.sw(0x7F, value)
        }
        fn fl(mut self, eflags: u32) -> Self {
            self.eflags = Some(eflags);
            self
        }
        fn near(mut self, ulps: u64) -> Self {
            self.ulps = ulps;
            self
        }
    }

    fn setup(case: &Case) -> (Cpu, Mem) {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        cpu.set_real_segment(CS, (CODE >> 4) as u16);
        cpu.set_real_segment(DS, 0);
        cpu.ip = 0;
        cpu.fpu.control = case.control;
        for value in case.stack.iter().rev() {
            cpu.fpu.push_raw(F80 { st: *value });
        }
        mem.data[CODE..CODE + case.code.len()].copy_from_slice(&case.code);
        mem.data[OPERAND..OPERAND + case.memory.len()].copy_from_slice(&case.memory);
        (cpu, mem)
    }

    /// The distance between two values with the same sign and neighbouring
    /// exponents, in units of the last place.
    fn ulps_apart(a: u128, b: u128) -> u128 {
        let to_line = |v: u128| (v >> 79, (v & ((1 << 79) - 1)) as i128);
        let ((sign_a, a), (sign_b, b)) = (to_line(a), to_line(b));
        if sign_a != sign_b {
            return u128::MAX;
        }
        // Moving up an exponent passes through the integer bit's 2^63 units.
        let squash = |v: i128| ((v >> 64) << 63) + (v & 0x7FFF_FFFF_FFFF_FFFF);
        squash(a).abs_diff(squash(b))
    }

    fn check(case: &Case) {
        let (mut cpu, mut mem) = setup(case);
        assert_eq!(cpu.step(&mut mem), Step::Ok, "{}: step", case.name);
        if let Some(want) = &case.want_stack {
            let depth = (0..8u8).filter(|i| !cpu.fpu.is_empty(*i)).count();
            assert_eq!(depth, want.len(), "{}: stack depth", case.name);
            for (index, value) in want.iter().enumerate() {
                let got = cpu.fpu.get(index as u8).st;
                let good = got == *value || ulps_apart(got, *value) <= case.ulps as u128;
                assert!(
                    good,
                    "{}: ST({index}) is {got:020X}, wanted {value:020X}",
                    case.name
                );
            }
        }
        let status = cpu.fpu.status_word();
        assert_eq!(
            status & case.status_mask,
            case.status,
            "{}: status {status:04X}",
            case.name
        );
        if !case.want_memory.is_empty() {
            let end = OPERAND + case.want_memory.len();
            assert_eq!(
                &mem.data[OPERAND..end],
                &case.want_memory[..],
                "{}: memory",
                case.name
            );
        }
        if let Some(want) = case.eflags {
            let mask = FLAG_ZF | FLAG_PF | FLAG_CF | FLAG_OF | FLAG_SF | FLAG_AF;
            assert_eq!(cpu.flags() & mask, want, "{}: eflags", case.name);
        }
    }

    fn check_all(cases: &[Case]) {
        for row in cases {
            check(row);
        }
    }

    /// A memory form at [0x0200].
    fn at(opcode: u8, reg: u8) -> [u8; 4] {
        [opcode, (reg << 3) | 6, 0x00, 0x02]
    }

    #[test]
    fn d8_register_forms() {
        let a = &[i(7), i(2)][..];
        check_all(&[
            case("FADD ST,ST(1)", &[0xD8, 0xC1])
                .st(a)
                .want(&[i(9), i(2)]),
            case("FMUL ST,ST(1)", &[0xD8, 0xC9])
                .st(a)
                .want(&[i(14), i(2)]),
            case("FSUB ST,ST(1)", &[0xD8, 0xE1])
                .st(a)
                .want(&[i(5), i(2)]),
            case("FSUBR ST,ST(1)", &[0xD8, 0xE9])
                .st(a)
                .want(&[i(-5), i(2)]),
            case("FDIV ST,ST(1)", &[0xD8, 0xF1])
                .st(&[i(7), i(2)])
                .want(&[0x4000_E000_0000_0000_0000, i(2)]),
            case("FDIVR ST,ST(1)", &[0xD8, 0xF9])
                .st(&[i(2), i(8)])
                .want(&[i(4), i(8)]),
            case("FCOM ST(1)", &[0xD8, 0xD1])
                .st(a)
                .want(a)
                .sw(C0 | C2 | C3, 0),
            case("FCOMP ST(1)", &[0xD8, 0xD9])
                .st(&[i(1), i(2)])
                .want(&[i(2)])
                .sw(C0 | C2 | C3, C0),
        ]);
    }

    #[test]
    fn dc_and_de_reversed_forms() {
        // ST = 7, ST(1) = 2. The DC and DE E0+i, E8+i, F0+i, F8+i forms write ST(i).
        let s = &[i(7), i(2)][..];
        let d = &[i(2), i(8)][..];
        check_all(&[
            case("FADD ST(1),ST", &[0xDC, 0xC1])
                .st(s)
                .want(&[i(7), i(9)]),
            case("FMUL ST(1),ST", &[0xDC, 0xC9])
                .st(s)
                .want(&[i(7), i(14)]),
            case("FSUBR ST(1),ST", &[0xDC, 0xE1])
                .st(s)
                .want(&[i(7), i(5)]),
            case("FSUB ST(1),ST", &[0xDC, 0xE9])
                .st(s)
                .want(&[i(7), i(-5)]),
            case("FDIVR ST(1),ST", &[0xDC, 0xF1])
                .st(d)
                .want(&[i(2), 0x3FFD_8000_0000_0000_0000]),
            case("FDIV ST(1),ST", &[0xDC, 0xF9])
                .st(d)
                .want(&[i(2), i(4)]),
            case("FADDP", &[0xDE, 0xC1]).st(s).want(&[i(9)]),
            case("FMULP", &[0xDE, 0xC9]).st(s).want(&[i(14)]),
            case("FSUBRP", &[0xDE, 0xE1]).st(s).want(&[i(5)]),
            case("FSUBP", &[0xDE, 0xE9]).st(s).want(&[i(-5)]),
            case("FDIVRP", &[0xDE, 0xF1])
                .st(&[i(8), i(2)])
                .want(&[i(4)]),
            case("FDIVP", &[0xDE, 0xF9]).st(d).want(&[i(4)]),
            // A higher register is the one written, not the top.
            case("FSUBP ST(2),ST", &[0xDE, 0xEA])
                .st(&[i(1), i(50), i(3)])
                .want(&[i(50), i(2)]),
        ]);
    }

    #[test]
    fn memory_arithmetic() {
        let two_f32 = 2f32.to_le_bytes();
        let two_f64 = 2f64.to_le_bytes();
        let two_i16 = 2i16.to_le_bytes();
        let two_i32 = 2i32.to_le_bytes();
        let s = &[i(8)][..];
        let mut rows = Vec::new();
        let forms: [(u8, &[u8]); 4] = [
            (0xD8, &two_f32),
            (0xDC, &two_f64),
            (0xDA, &two_i32),
            (0xDE, &two_i16),
        ];
        for (opcode, operand) in forms {
            rows.push(
                case("add", &at(opcode, 0))
                    .st(s)
                    .mem(operand)
                    .want(&[i(10)]),
            );
            rows.push(
                case("mul", &at(opcode, 1))
                    .st(s)
                    .mem(operand)
                    .want(&[i(16)]),
            );
            rows.push(
                case("com", &at(opcode, 2))
                    .st(s)
                    .mem(operand)
                    .want(s)
                    .sw(C0 | C2 | C3, 0),
            );
            rows.push(
                case("comp", &at(opcode, 3))
                    .st(s)
                    .mem(operand)
                    .want(&[])
                    .sw(C0 | C2 | C3, 0),
            );
            rows.push(case("sub", &at(opcode, 4)).st(s).mem(operand).want(&[i(6)]));
            rows.push(
                case("subr", &at(opcode, 5))
                    .st(s)
                    .mem(operand)
                    .want(&[i(-6)]),
            );
            rows.push(case("div", &at(opcode, 6)).st(s).mem(operand).want(&[i(4)]));
            rows.push(
                case("divr", &at(opcode, 7))
                    .st(&[i(1)])
                    .mem(operand)
                    .want(&[i(2)]),
            );
        }
        check_all(&rows);
    }

    #[test]
    fn stores_to_registers_copy_the_top() {
        check_all(&[
            case("FST ST(1)", &[0xDD, 0xD1])
                .st(&[i(1), i(2)])
                .want(&[i(1), i(1)]),
            case("FSTP ST(1)", &[0xDD, 0xD9])
                .st(&[i(1), i(2)])
                .want(&[i(1)]),
            case("FSTP ST(2)", &[0xDD, 0xDA])
                .st(&[i(1), i(2), i(3)])
                .want(&[i(2), i(1)]),
            case("FLD ST(1)", &[0xD9, 0xC1])
                .st(&[i(1), i(2)])
                .want(&[i(2), i(1), i(2)]),
            case("FXCH ST(1)", &[0xD9, 0xC9])
                .st(&[i(1), i(2)])
                .want(&[i(2), i(1)]),
        ]);
    }

    #[test]
    fn compares_set_condition_codes() {
        let cc = C0 | C2 | C3;
        check_all(&[
            case("FCOM >", &[0xD8, 0xD1]).st(&[i(3), i(2)]).sw(cc, 0),
            case("FCOM <", &[0xD8, 0xD1]).st(&[i(1), i(2)]).sw(cc, C0),
            case("FCOM =", &[0xD8, 0xD1]).st(&[i(2), i(2)]).sw(cc, C3),
            case("FCOM +0 -0", &[0xD8, 0xD1])
                .st(&[POS_ZERO, NEG_ZERO])
                .sw(cc, C3),
            case("FCOM +Inf > x", &[0xD8, 0xD1])
                .st(&[POS_INF, i(2)])
                .sw(cc | 1, 0),
            case("FCOM -Inf < x", &[0xD8, 0xD1])
                .st(&[NEG_INF, i(2)])
                .sw(cc | 1, C0),
            case("FCOM x > -Inf", &[0xD8, 0xD1])
                .st(&[i(2), NEG_INF])
                .sw(cc | 1, 0),
            case("FCOM Inf = Inf", &[0xD8, 0xD1])
                .st(&[POS_INF, POS_INF])
                .sw(cc | 1, C3),
            case("FCOM QNaN", &[0xD8, 0xD1])
                .st(&[QNAN, i(2)])
                .sw(cc | 1, C0 | C2 | C3 | SW_IE),
            case("FUCOM QNaN", &[0xDD, 0xE1])
                .st(&[QNAN, i(2)])
                .sw(cc | 1, C0 | C2 | C3),
            case("FUCOM SNaN", &[0xDD, 0xE1])
                .st(&[SNAN, i(2)])
                .sw(cc | 1, C0 | C2 | C3 | SW_IE),
            case("FUCOMP", &[0xDD, 0xE9])
                .st(&[i(1), i(2)])
                .want(&[i(2)])
                .sw(cc, C0),
            case("FCOMPP pops twice", &[0xDE, 0xD9])
                .st(&[i(1), i(2), i(9)])
                .want(&[i(9)])
                .sw(cc, C0),
            case("FUCOMPP pops twice", &[0xDA, 0xE9])
                .st(&[i(2), i(2), i(9)])
                .want(&[i(9)])
                .sw(cc, C3),
            case("FTST >", &[0xD9, 0xE4]).st(&[i(2)]).sw(cc, 0),
            case("FTST <", &[0xD9, 0xE4]).st(&[i(-2)]).sw(cc, C0),
            case("FTST =", &[0xD9, 0xE4]).st(&[NEG_ZERO]).sw(cc, C3),
            case("FTST NaN", &[0xD9, 0xE4])
                .st(&[QNAN])
                .sw(cc | 1, cc | SW_IE),
            case("FCOM denormal", &[0xD8, 0xD1])
                .st(&[DENORMAL, i(2)])
                .sw(cc | 2, C0 | SW_DE),
            case("FCOMI >", &[0xDB, 0xF1]).st(&[i(3), i(2)]).fl(0),
            case("FCOMI <", &[0xDB, 0xF1]).st(&[i(1), i(2)]).fl(FLAG_CF),
            case("FCOMI =", &[0xDB, 0xF1]).st(&[i(2), i(2)]).fl(FLAG_ZF),
            case("FCOMI NaN", &[0xDB, 0xF1])
                .st(&[QNAN, i(2)])
                .fl(FLAG_ZF | FLAG_PF | FLAG_CF)
                .ex(SW_IE),
            case("FUCOMI NaN", &[0xDB, 0xE9])
                .st(&[QNAN, i(2)])
                .fl(FLAG_ZF | FLAG_PF | FLAG_CF)
                .ex(0),
            case("FCOMIP", &[0xDF, 0xF1])
                .st(&[i(1), i(2)])
                .want(&[i(2)])
                .fl(FLAG_CF),
            case("FUCOMIP", &[0xDF, 0xE9])
                .st(&[i(2), i(2)])
                .want(&[i(2)])
                .fl(FLAG_ZF),
        ]);
    }

    #[test]
    fn fxam_reports_every_class() {
        let cc = C0 | C1 | C2 | C3;
        let fxam = [0xD9, 0xE5];
        check_all(&[
            case("unsupported", &fxam).st(&[UNNORMAL]).sw(cc, 0),
            case("NaN", &fxam).st(&[QNAN]).sw(cc, C0),
            case("SNaN", &fxam).st(&[SNAN]).sw(cc, C0),
            case("normal", &fxam).st(&[i(1)]).sw(cc, C2),
            case("negative normal", &fxam).st(&[i(-1)]).sw(cc, C2 | C1),
            case("infinity", &fxam).st(&[POS_INF]).sw(cc, C2 | C0),
            case("negative infinity", &fxam)
                .st(&[NEG_INF])
                .sw(cc, C2 | C0 | C1),
            case("zero", &fxam).st(&[POS_ZERO]).sw(cc, C3),
            case("negative zero", &fxam).st(&[NEG_ZERO]).sw(cc, C3 | C1),
            case("empty", &fxam).sw(C0 | C2 | C3, C3 | C0),
            case("denormal", &fxam).st(&[DENORMAL]).sw(cc, C3 | C2),
        ]);
    }

    #[test]
    fn frndint_honours_rounding_control() {
        let f = [0xD9, 0xFC];
        check_all(&[
            case("nearest 2.5", &f)
                .st(&[TWO_POINT_FIVE])
                .want(&[i(2)])
                .sw(C1 | 0x7F, SW_PE),
            case("nearest 1.5", &f)
                .st(&[ONE_POINT_FIVE])
                .want(&[i(2)])
                .sw(C1 | 0x7F, SW_PE | C1),
            case("down 2.5", &f)
                .cw(DOWN)
                .st(&[TWO_POINT_FIVE])
                .want(&[i(2)]),
            case("up 2.5", &f)
                .cw(UP)
                .st(&[TWO_POINT_FIVE])
                .want(&[i(3)]),
            case("chop 2.5", &f)
                .cw(CHOP)
                .st(&[TWO_POINT_FIVE])
                .want(&[i(2)]),
            case("down -2.5", &f)
                .cw(DOWN)
                .st(&[MINUS_2_5])
                .want(&[i(-3)]),
            case("up -2.5", &f).cw(UP).st(&[MINUS_2_5]).want(&[i(-2)]),
            case("chop -2.5", &f)
                .cw(CHOP)
                .st(&[MINUS_2_5])
                .want(&[i(-2)]),
            case("exact", &f).st(&[i(5)]).want(&[i(5)]).ex(0),
            case("2^63 stays", &f).st(&[TWO_63]).want(&[TWO_63]).ex(0),
            case("2^70 stays", &f).st(&[TWO_70]).want(&[TWO_70]).ex(0),
            case("Inf stays", &f).st(&[POS_INF]).want(&[POS_INF]).ex(0),
            case("-Inf stays", &f).st(&[NEG_INF]).want(&[NEG_INF]).ex(0),
            case("NaN stays", &f).st(&[QNAN]).want(&[QNAN]).ex(0),
            case("SNaN quiets", &f)
                .st(&[SNAN])
                .want(&[SNAN | (1 << 62)])
                .ex(SW_IE),
            case("-0 stays", &f).st(&[NEG_ZERO]).want(&[NEG_ZERO]).ex(0),
            case("-0.25 to -0", &f)
                .st(&[0xBFFD_8000_0000_0000_0000])
                .want(&[NEG_ZERO])
                .ex(SW_PE),
            case("-0.25 down", &f)
                .cw(DOWN)
                .st(&[0xBFFD_8000_0000_0000_0000])
                .want(&[i(-1)]),
            case("denormal up", &f)
                .cw(UP)
                .st(&[DENORMAL])
                .want(&[i(1)])
                .ex(SW_DE | SW_PE),
        ]);
    }

    #[test]
    fn fist_honours_rounding_control_and_range() {
        let i16_out = |v: i16| v.to_le_bytes().to_vec();
        let i32_out = |v: i32| v.to_le_bytes().to_vec();
        let i64_out = |v: i64| v.to_le_bytes().to_vec();
        let (fist16, fistp16) = (at(0xDF, 2), at(0xDF, 3));
        let (fist32, fistp32) = (at(0xDB, 2), at(0xDB, 3));
        let fistp64 = at(0xDF, 7);
        let v = TWO_POINT_FIVE;
        check_all(&[
            case("16 nearest", &fist16)
                .st(&[v])
                .out(&i16_out(2))
                .want(&[v])
                .ex(SW_PE),
            case("16 down", &fist16).cw(DOWN).st(&[v]).out(&i16_out(2)),
            case("16 up", &fist16)
                .cw(UP)
                .st(&[v])
                .out(&i16_out(3))
                .sw(C1, C1),
            case("16 chop", &fist16)
                .cw(CHOP)
                .st(&[MINUS_2_5])
                .out(&i16_out(-2)),
            case("16 pops", &fistp16)
                .st(&[i(-7), i(1)])
                .out(&i16_out(-7))
                .want(&[i(1)])
                .ex(0),
            case("16 min", &fist16)
                .st(&[i(-32768)])
                .out(&i16_out(-32768))
                .ex(0),
            case("16 max+1", &fist16)
                .st(&[i(32768)])
                .out(&i16_out(-32768))
                .ex(SW_IE),
            case("16 min-1", &fist16)
                .st(&[i(-32769)])
                .out(&i16_out(-32768))
                .ex(SW_IE),
            case("32 nearest", &fist32)
                .st(&[i(100000)])
                .out(&i32_out(100000))
                .ex(0),
            case("32 pops", &fistp32)
                .st(&[MINUS_2_5])
                .cw(DOWN)
                .out(&i32_out(-3))
                .want(&[]),
            case("32 out", &fist32)
                .st(&[TWO_63])
                .out(&i32_out(i32::MIN))
                .ex(SW_IE),
            case("32 Inf", &fist32)
                .st(&[POS_INF])
                .out(&i32_out(i32::MIN))
                .ex(SW_IE),
            case("32 NaN", &fist32)
                .st(&[QNAN])
                .out(&i32_out(i32::MIN))
                .ex(SW_IE),
            case("64 exact", &fistp64)
                .st(&[i(0x1234_5678_9ABC_DEF1)])
                .out(&i64_out(0x1234_5678_9ABC_DEF1))
                .ex(0),
            case("64 min", &fistp64)
                .st(&[i(i64::MIN)])
                .out(&i64_out(i64::MIN))
                .ex(0),
            case("64 2^63", &fistp64)
                .st(&[TWO_63])
                .out(&i64_out(i64::MIN))
                .ex(SW_IE),
            case("64 2^70", &fistp64)
                .st(&[TWO_70])
                .out(&i64_out(i64::MIN))
                .ex(SW_IE),
            case("64 up", &fistp64).cw(UP).st(&[v]).out(&i64_out(3)),
            case("empty", &fistp16)
                .out(&i16_out(-32768))
                .sw(0x7F | C1, SW_IE | SW_SF),
            case("FISTTP chops", &at(0xDF, 1))
                .cw(UP)
                .st(&[v])
                .out(&i16_out(2))
                .want(&[]),
        ]);
    }

    #[test]
    fn bcd_loads_and_stores() {
        let digits = [0x89, 0x67, 0x45, 0x23, 0x01, 0, 0, 0, 0, 0];
        let negative = [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0x80];
        let most = [0x99, 0x99, 0x99, 0x99, 0x99, 0x99, 0x99, 0x99, 0x99, 0];
        let most_value = 999_999_999_999_999_999i64;
        let store = at(0xDF, 6);
        check_all(&[
            case("FBLD", &at(0xDF, 4))
                .mem(&digits)
                .want(&[i(123456789)])
                .ex(0),
            case("FBLD neg", &at(0xDF, 4))
                .mem(&negative)
                .want(&[i(-25)]),
            case("FBLD -0", &at(0xDF, 4))
                .mem(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0x80])
                .want(&[NEG_ZERO]),
            case("FBLD 18 digits", &at(0xDF, 4))
                .mem(&most)
                .want(&[i(most_value)]),
            case("FBSTP", &store)
                .st(&[i(123456789)])
                .out(&digits)
                .want(&[])
                .ex(0),
            case("FBSTP neg", &store).st(&[i(-25)]).out(&negative),
            case("FBSTP 18 digits", &store)
                .st(&[i(most_value)])
                .out(&most)
                .ex(0),
            case("FBSTP nearest", &store)
                .st(&[TWO_POINT_FIVE])
                .out(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                .ex(SW_PE),
            case("FBSTP up", &store)
                .cw(UP)
                .st(&[TWO_POINT_FIVE])
                .out(&[3, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            case("FBSTP chop", &store)
                .cw(CHOP)
                .st(&[MINUS_2_5])
                .out(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0x80]),
            case("FBSTP down", &store)
                .cw(DOWN)
                .st(&[MINUS_2_5])
                .out(&[3, 0, 0, 0, 0, 0, 0, 0, 0, 0x80]),
            case("FBSTP 10^18", &store)
                .st(&[i(most_value + 1)])
                .out(&BCD_INDEFINITE)
                .ex(SW_IE),
            case("FBSTP Inf", &store)
                .st(&[POS_INF])
                .out(&BCD_INDEFINITE)
                .ex(SW_IE),
            case("FBSTP NaN", &store)
                .st(&[QNAN])
                .out(&BCD_INDEFINITE)
                .ex(SW_IE),
            case("FBSTP empty", &store)
                .out(&BCD_INDEFINITE)
                .ex(SW_IE | SW_SF),
        ]);
    }

    #[test]
    fn real_loads_widen_exactly() {
        let f32_bits = |v: u32| v.to_le_bytes().to_vec();
        let f64_bits = |v: u64| v.to_le_bytes().to_vec();
        let (m32, m64) = (at(0xD9, 0), at(0xDD, 0));
        check_all(&[
            case("m32 1.5", &m32)
                .mem(&1.5f32.to_le_bytes())
                .want(&[ONE_POINT_FIVE])
                .ex(0),
            case("m64 1.5", &m64)
                .mem(&1.5f64.to_le_bytes())
                .want(&[ONE_POINT_FIVE])
                .ex(0),
            case("m32 -0", &m32)
                .mem(&f32_bits(0x8000_0000))
                .want(&[NEG_ZERO]),
            case("m64 -0", &m64)
                .mem(&f64_bits(1 << 63))
                .want(&[NEG_ZERO]),
            case("m32 Inf", &m32)
                .mem(&f32_bits(0x7F80_0000))
                .want(&[POS_INF])
                .ex(0),
            case("m64 -Inf", &m64)
                .mem(&f64_bits(0xFFF0_0000_0000_0000))
                .want(&[NEG_INF]),
            case("m32 QNaN", &m32)
                .mem(&f32_bits(0x7FC0_0001))
                .want(&[0x7FFF_C000_0100_0000_0000])
                .ex(0),
            case("m32 SNaN quiets", &m32)
                .mem(&f32_bits(0x7F80_0001))
                .want(&[0x7FFF_C000_0100_0000_0000])
                .ex(SW_IE),
            case("m64 SNaN quiets", &m64)
                .mem(&f64_bits(0x7FF0_0000_0000_0001))
                .want(&[0x7FFF_C000_0000_0000_0800])
                .ex(SW_IE),
            case("m32 denormal normalizes", &m32)
                .mem(&f32_bits(1))
                .want(&[0x3F6A_8000_0000_0000_0000])
                .ex(SW_DE),
            case("m64 denormal normalizes", &m64)
                .mem(&f64_bits(1))
                .want(&[0x3BCD_8000_0000_0000_0000])
                .ex(SW_DE),
            case("m80 keeps patterns", &at(0xDB, 5))
                .mem(&F80 { st: SNAN }.get_bytes())
                .want(&[SNAN])
                .ex(0),
            case("FILD m16", &at(0xDF, 0))
                .mem(&(-5i16).to_le_bytes())
                .want(&[i(-5)]),
            case("FILD m32", &at(0xDB, 0))
                .mem(&(-70000i32).to_le_bytes())
                .want(&[i(-70000)]),
            case("FILD m64 is exact", &at(0xDF, 5))
                .mem(&0x7FFF_FFFF_FFFF_FFFFi64.to_le_bytes())
                .want(&[0x403D_FFFF_FFFF_FFFF_FFFE]),
            case("FILD m64 min", &at(0xDF, 5))
                .mem(&i64::MIN.to_le_bytes())
                .want(&[0xC03E_8000_0000_0000_0000]),
        ]);
    }

    #[test]
    fn real_stores_round() {
        let f32_bits = |v: u32| v.to_le_bytes().to_vec();
        let f64_bits = |v: u64| v.to_le_bytes().to_vec();
        let (st32, st64) = (at(0xD9, 2), at(0xDD, 2));
        let (stp32, stp64) = (at(0xD9, 3), at(0xDD, 3));
        // 1 + 2^-30, which a single can't hold.
        let just_over_one = 0x3FFF_8000_0004_0000_0000;
        let minus = just_over_one | (1 << 79);
        check_all(&[
            case("m32 nearest", &st32)
                .st(&[just_over_one])
                .out(&f32_bits(0x3F80_0000))
                .want(&[just_over_one])
                .ex(SW_PE),
            case("m32 up", &st32)
                .cw(UP)
                .st(&[just_over_one])
                .out(&f32_bits(0x3F80_0001))
                .sw(C1 | 0x7F, SW_PE | C1),
            case("m32 down", &st32)
                .cw(DOWN)
                .st(&[just_over_one])
                .out(&f32_bits(0x3F80_0000)),
            case("m32 chop", &st32)
                .cw(CHOP)
                .st(&[minus])
                .out(&f32_bits(0xBF80_0000)),
            case("m32 down neg", &st32)
                .cw(DOWN)
                .st(&[minus])
                .out(&f32_bits(0xBF80_0001)),
            case("m32 pops", &stp32)
                .st(&[i(1), i(2)])
                .out(&f32_bits(0x3F80_0000))
                .want(&[i(2)])
                .ex(0),
            case("m32 overflow", &st32)
                .st(&[LARGEST])
                .out(&f32_bits(0x7F80_0000))
                .ex(SW_OE | SW_PE),
            case("m32 overflow chop", &st32)
                .cw(CHOP)
                .st(&[LARGEST])
                .out(&f32_bits(0x7F7F_FFFF))
                .ex(SW_OE | SW_PE),
            // 2^-149 is the smallest denormal; three quarters of it rounds up.
            case("m32 denormal", &st32)
                .st(&[0x3F69_8000_0000_0000_0000 | (1 << 62)])
                .out(&f32_bits(1))
                .ex(SW_UE | SW_PE),
            case("m32 exact denormal", &st32)
                .st(&[0x3F6A_8000_0000_0000_0000])
                .out(&f32_bits(1))
                .ex(0),
            case("m32 underflow", &st32)
                .st(&[0x0001_8000_0000_0000_0000])
                .out(&f32_bits(0))
                .ex(SW_UE | SW_PE),
            case("m32 underflow up", &st32)
                .cw(UP)
                .st(&[0x0001_8000_0000_0000_0000])
                .out(&f32_bits(1)),
            case("m32 NaN", &st32)
                .st(&[QNAN])
                .out(&f32_bits(0x7FC0_0000))
                .ex(0),
            case("m32 SNaN", &st32)
                .st(&[SNAN])
                .out(&f32_bits(0x7FC0_0000))
                .ex(SW_IE),
            case("m32 Inf", &st32)
                .st(&[NEG_INF])
                .out(&f32_bits(0xFF80_0000))
                .ex(0),
            case("m32 empty", &st32)
                .out(&f32_bits(0xFFC0_0000))
                .ex(SW_IE | SW_SF),
            case("m64 exact", &st64)
                .st(&[ONE_POINT_FIVE])
                .out(&1.5f64.to_le_bytes())
                .ex(0),
            case("m64 nearest", &st64)
                .st(&[ONE_PLUS])
                .out(&f64_bits(0x3FF0_0000_0000_0000))
                .ex(SW_PE),
            case("m64 up", &st64)
                .cw(UP)
                .st(&[ONE_PLUS])
                .out(&f64_bits(0x3FF0_0000_0000_0001)),
            case("m64 pops", &stp64)
                .st(&[i(1)])
                .out(&1f64.to_le_bytes())
                .want(&[]),
            case("m64 overflow", &st64)
                .st(&[LARGEST])
                .out(&f64_bits(0x7FF0_0000_0000_0000))
                .ex(SW_OE | SW_PE),
            case("m64 overflow down", &st64)
                .cw(DOWN)
                .st(&[LARGEST])
                .out(&f64_bits(0x7FEF_FFFF_FFFF_FFFF))
                .ex(SW_OE | SW_PE),
            case("m64 denormal", &st64)
                .st(&[0x3BCD_8000_0000_0000_0000])
                .out(&f64_bits(1))
                .ex(0),
            case("m64 Inf", &st64)
                .st(&[POS_INF])
                .out(&f64_bits(0x7FF0_0000_0000_0000)),
            case("m64 empty", &st64)
                .out(&f64_bits(0xFFF8_0000_0000_0000))
                .ex(SW_IE | SW_SF),
            case("m80", &at(0xDB, 7))
                .st(&[SNAN])
                .out(&F80 { st: SNAN }.get_bytes())
                .want(&[])
                .ex(0),
            case("m80 empty", &at(0xDB, 7))
                .out(&F80 { st: INDEFINITE }.get_bytes())
                .ex(SW_IE | SW_SF),
        ]);
    }

    #[test]
    fn eighty_bit_arithmetic_rounds() {
        let (add, mul, div) = ([0xD8, 0xC1], [0xD8, 0xC9], [0xD8, 0xF1]);
        let third_low = 0x3FFD_AAAA_AAAA_AAAA_AAAA;
        let third_high = 0x3FFD_AAAA_AAAA_AAAA_AAAB;
        // 1 + 2^-63 keeps its last bit, which a double would lose.
        let tiny = 0x3FC0_8000_0000_0000_0000;
        // The product of 1 + 2^-63 with itself has a last bit to round.
        let sq_up = 0x3FFF_8000_0000_0000_0003;
        let pc53: u16 = 0x027F;
        let pc24: u16 = 0x007F;
        check_all(&[
            case("add keeps 64 bits", &add)
                .st(&[i(1), tiny])
                .want(&[ONE_PLUS, tiny])
                .ex(0),
            case("mul nearest", &mul)
                .st(&[ONE_PLUS, ONE_PLUS])
                .want(&[0x3FFF_8000_0000_0000_0002, ONE_PLUS])
                .ex(SW_PE),
            case("mul up", &mul)
                .cw(UP)
                .st(&[ONE_PLUS, ONE_PLUS])
                .want(&[sq_up, ONE_PLUS])
                .sw(C1 | 0x7F, SW_PE | C1),
            case("mul down", &mul)
                .cw(DOWN)
                .st(&[ONE_PLUS, ONE_PLUS])
                .want(&[0x3FFF_8000_0000_0000_0002, ONE_PLUS]),
            case("div nearest", &div)
                .st(&[i(1), i(3)])
                .want(&[third_high, i(3)])
                .sw(C1 | 0x7F, SW_PE | C1),
            case("div down", &div)
                .cw(DOWN)
                .st(&[i(1), i(3)])
                .want(&[third_low, i(3)])
                .sw(C1 | 0x7F, SW_PE),
            case("div up", &div)
                .cw(UP)
                .st(&[i(1), i(3)])
                .want(&[third_high, i(3)]),
            case("div chop", &div)
                .cw(CHOP)
                .st(&[i(-1), i(3)])
                .want(&[third_low | (1 << 79), i(3)]),
            case("div down neg", &div)
                .cw(DOWN)
                .st(&[i(-1), i(3)])
                .want(&[third_high | (1 << 79), i(3)]),
            case("PC 53", &div)
                .cw(pc53)
                .st(&[i(1), i(3)])
                .want(&[0x3FFD_AAAA_AAAA_AAAA_A800, i(3)]),
            case("PC 24", &div)
                .cw(pc24)
                .st(&[i(1), i(3)])
                .want(&[0x3FFD_AAAA_AB00_0000_0000, i(3)]),
            case("PC 24 add", &add)
                .cw(pc24)
                .st(&[ONE_PLUS, ONE_PLUS])
                .want(&[i(2), ONE_PLUS])
                .ex(SW_PE),
            case("PC doesn't touch FLD", &[0xD9, 0xC1])
                .cw(pc24)
                .st(&[ONE_PLUS, ONE_PLUS])
                .want(&[ONE_PLUS, ONE_PLUS, ONE_PLUS])
                .ex(0),
            case("sqrt PC 24", &[0xD9, 0xFA])
                .cw(pc24)
                .st(&[i(2)])
                .want(&[0x3FFF_B504_F300_0000_0000]),
        ]);
    }

    #[test]
    fn specials_and_exception_flags() {
        let (add, sub, mul, div) = ([0xD8, 0xC1], [0xD8, 0xE1], [0xD8, 0xC9], [0xD8, 0xF1]);
        check_all(&[
            case("x / 0", &div)
                .st(&[i(1), POS_ZERO])
                .want(&[POS_INF, POS_ZERO])
                .ex(SW_ZE),
            case("-x / 0", &div)
                .st(&[i(-1), POS_ZERO])
                .want(&[NEG_INF, POS_ZERO])
                .ex(SW_ZE),
            case("0 / 0", &div)
                .st(&[POS_ZERO, POS_ZERO])
                .want(&[INDEFINITE, POS_ZERO])
                .ex(SW_IE),
            case("Inf / Inf", &div)
                .st(&[POS_INF, POS_INF])
                .want(&[INDEFINITE, POS_INF])
                .ex(SW_IE),
            case("x / Inf", &div)
                .st(&[i(1), NEG_INF])
                .want(&[NEG_ZERO, NEG_INF])
                .ex(0),
            case("Inf - Inf", &sub)
                .st(&[POS_INF, POS_INF])
                .want(&[INDEFINITE, POS_INF])
                .ex(SW_IE),
            case("Inf + -Inf", &add)
                .st(&[POS_INF, NEG_INF])
                .want(&[INDEFINITE, NEG_INF])
                .ex(SW_IE),
            case("Inf * 0", &mul)
                .st(&[POS_INF, POS_ZERO])
                .want(&[INDEFINITE, POS_ZERO])
                .ex(SW_IE),
            case("x - x", &sub)
                .st(&[i(3), i(3)])
                .want(&[POS_ZERO, i(3)])
                .ex(0),
            case("x - x down", &sub)
                .cw(DOWN)
                .st(&[i(3), i(3)])
                .want(&[NEG_ZERO, i(3)])
                .ex(0),
            case("-0 + -0", &add)
                .st(&[NEG_ZERO, NEG_ZERO])
                .want(&[NEG_ZERO, NEG_ZERO])
                .ex(0),
            case("+0 + -0", &add)
                .st(&[POS_ZERO, NEG_ZERO])
                .want(&[POS_ZERO, NEG_ZERO])
                .ex(0),
            case("overflow", &mul)
                .st(&[LARGEST, LARGEST])
                .want(&[POS_INF, LARGEST])
                .ex(SW_OE | SW_PE),
            case("overflow chop", &mul)
                .cw(CHOP)
                .st(&[LARGEST, LARGEST])
                .want(&[LARGEST, LARGEST])
                .ex(SW_OE | SW_PE),
            case("underflow", &mul)
                .st(&[SMALLEST_NORMAL, HALF])
                .want(&[0x0000_4000_0000_0000_0000, HALF])
                .ex(0),
            case("inexact underflow", &mul)
                .st(&[0x0001_C000_0000_0000_0001, HALF])
                .want(&[0x0000_6000_0000_0000_0000, HALF])
                .ex(SW_UE | SW_PE),
            case("denormal operand", &add)
                .st(&[DENORMAL, i(1)])
                .ex(SW_DE | SW_PE),
            case("denormal memory operand", &at(0xD8, 0))
                .st(&[i(1)])
                .mem(&1u32.to_le_bytes())
                .ex(SW_DE | SW_PE),
            case("SNaN", &add)
                .st(&[SNAN, i(1)])
                .want(&[SNAN | (1 << 62), i(1)])
                .ex(SW_IE),
            case("QNaN", &add)
                .st(&[QNAN, i(1)])
                .want(&[QNAN, i(1)])
                .ex(0),
            case("QNaN beats SNaN", &add)
                .st(&[SNAN, QNAN])
                .want(&[QNAN, QNAN])
                .ex(SW_IE),
            case("bigger QNaN", &add)
                .st(&[0x7FFF_C000_0000_0000_0001, 0x7FFF_C000_0000_0000_0002])
                .want(&[0x7FFF_C000_0000_0000_0002, 0x7FFF_C000_0000_0000_0002])
                .ex(0),
            case("unsupported", &add)
                .st(&[UNNORMAL, i(1)])
                .want(&[INDEFINITE, i(1)])
                .ex(SW_IE),
            case("precision", &div).st(&[i(1), i(3)]).ex(SW_PE),
        ]);
    }

    #[test]
    fn stack_faults_take_the_masked_response() {
        let full: Vec<u128> = (1..=8).map(i).collect();
        let mut after_load = alloc::vec![INDEFINITE];
        after_load.extend_from_slice(&full[..7]);
        let flags = 0x7F | C1;
        check_all(&[
            case("FLD onto a full stack", &[0xD9, 0xC1])
                .st(&full)
                .want(&[INDEFINITE, i(1), i(2), i(3), i(4), i(5), i(6), i(7)])
                .sw(flags, SW_IE | SW_SF | C1),
            case("FLD1 onto a full stack", &[0xD9, 0xE8])
                .st(&full)
                .want(&after_load)
                .sw(flags, SW_IE | SW_SF | C1),
            case("FLD m32 onto a full stack", &at(0xD9, 0))
                .st(&full)
                .mem(&1f32.to_le_bytes())
                .want(&after_load)
                .sw(flags, SW_IE | SW_SF | C1),
            case("FILD onto a full stack", &at(0xDF, 0))
                .st(&full)
                .want(&after_load)
                .sw(flags, SW_IE | SW_SF | C1),
            case("FLD empty", &[0xD9, 0xC1])
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FADD empty ST", &[0xD8, 0xC1])
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FADD empty ST(1)", &[0xD8, 0xC1])
                .st(&[i(1)])
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FADDP empty", &[0xDE, 0xC1])
                .st(&[i(1)])
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FADD m32 empty", &at(0xD8, 0))
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FCOM empty", &[0xD8, 0xD1])
                .sw(flags | C0 | C2 | C3, SW_IE | SW_SF | C0 | C2 | C3),
            case("FCOMPP empty", &[0xDE, 0xD9])
                .st(&[i(1)])
                .want(&[])
                .sw(flags | C0 | C2 | C3, SW_IE | SW_SF | C0 | C2 | C3),
            case("FSTP empty", &[0xDD, 0xD9])
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FXCH empty", &[0xD9, 0xC9])
                .st(&[i(1)])
                .want(&[INDEFINITE, i(1)])
                .sw(flags, SW_IE | SW_SF),
            case("FSQRT empty", &[0xD9, 0xFA])
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FCHS empty", &[0xD9, 0xE0])
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FSIN empty", &[0xD9, 0xFE])
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FPTAN empty", &[0xD9, 0xF2])
                .want(&[INDEFINITE, INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FYL2X empty", &[0xD9, 0xF1])
                .st(&[i(1)])
                .want(&[INDEFINITE])
                .sw(flags, SW_IE | SW_SF),
            case("FPTAN full", &[0xD9, 0xF2])
                .st(&full)
                .sw(flags, SW_IE | SW_SF | C1),
            case("FSINCOS full", &[0xD9, 0xFB])
                .st(&full)
                .sw(flags, SW_IE | SW_SF | C1),
            case("FXTRACT full", &[0xD9, 0xF4])
                .st(&full)
                .sw(flags, SW_IE | SW_SF | C1),
        ]);
    }

    #[test]
    fn constants_have_all_their_bits() {
        let pi = 0x4000_C90F_DAA2_2168_C235;
        let pi_low = 0x4000_C90F_DAA2_2168_C234;
        let l2t = 0x4000_D49A_784B_CD1B_8AFE;
        let l2t_up = 0x4000_D49A_784B_CD1B_8AFF;
        let l2e = 0x3FFF_B8AA_3B29_5C17_F0BC;
        let l2e_low = 0x3FFF_B8AA_3B29_5C17_F0BB;
        let lg2 = 0x3FFD_9A20_9A84_FBCF_F799;
        let lg2_low = 0x3FFD_9A20_9A84_FBCF_F798;
        let ln2 = 0x3FFE_B172_17F7_D1CF_79AC;
        let ln2_low = 0x3FFE_B172_17F7_D1CF_79AB;
        check_all(&[
            case("FLD1", &[0xD9, 0xE8]).want(&[i(1)]).ex(0),
            case("FLDZ", &[0xD9, 0xEE]).want(&[POS_ZERO]).ex(0),
            case("FLDPI", &[0xD9, 0xEB]).want(&[pi]).sw(0x7F | C1, 0),
            case("FLDPI up", &[0xD9, 0xEB]).cw(UP).want(&[pi]),
            case("FLDPI down", &[0xD9, 0xEB]).cw(DOWN).want(&[pi_low]),
            case("FLDPI chop", &[0xD9, 0xEB]).cw(CHOP).want(&[pi_low]),
            case("FLDL2T", &[0xD9, 0xE9]).want(&[l2t]),
            case("FLDL2T up", &[0xD9, 0xE9]).cw(UP).want(&[l2t_up]),
            case("FLDL2T down", &[0xD9, 0xE9]).cw(DOWN).want(&[l2t]),
            case("FLDL2E", &[0xD9, 0xEA]).want(&[l2e]),
            case("FLDL2E down", &[0xD9, 0xEA]).cw(DOWN).want(&[l2e_low]),
            case("FLDLG2", &[0xD9, 0xEC]).want(&[lg2]),
            case("FLDLG2 chop", &[0xD9, 0xEC]).cw(CHOP).want(&[lg2_low]),
            case("FLDLN2", &[0xD9, 0xED]).want(&[ln2]),
            case("FLDLN2 chop", &[0xD9, 0xED]).cw(CHOP).want(&[ln2_low]),
        ]);
    }

    #[test]
    fn fsqrt() {
        let f = [0xD9, 0xFA];
        check_all(&[
            case("sqrt 144", &f).st(&[i(144)]).want(&[i(12)]).ex(0),
            case("sqrt 1e20", &f)
                .st(&[0x4041_AD78_EBC5_AC62_0000])
                .want(&[0x4020_9502_F900_0000_0000])
                .ex(0),
            case("sqrt 2", &f)
                .st(&[i(2)])
                .want(&[0x3FFF_B504_F333_F9DE_6484])
                .ex(SW_PE),
            case("sqrt 2 up", &f)
                .cw(UP)
                .st(&[i(2)])
                .want(&[0x3FFF_B504_F333_F9DE_6485]),
            case("sqrt 2^-200", &f)
                .st(&[0x3F37_8000_0000_0000_0000])
                .want(&[0x3F9B_8000_0000_0000_0000])
                .ex(0),
            case("sqrt 2^200", &f)
                .st(&[0x40C7_8000_0000_0000_0000])
                .want(&[0x4063_8000_0000_0000_0000])
                .ex(0),
            case("sqrt denormal", &f)
                .st(&[0x0000_0000_0000_0000_0002])
                .want(&[0x1FE1_8000_0000_0000_0000])
                .ex(SW_DE),
            case("sqrt -0", &f).st(&[NEG_ZERO]).want(&[NEG_ZERO]).ex(0),
            case("sqrt -1", &f)
                .st(&[i(-1)])
                .want(&[INDEFINITE])
                .ex(SW_IE),
            case("sqrt Inf", &f).st(&[POS_INF]).want(&[POS_INF]).ex(0),
            case("sqrt -Inf", &f)
                .st(&[NEG_INF])
                .want(&[INDEFINITE])
                .ex(SW_IE),
            case("sqrt NaN", &f).st(&[QNAN]).want(&[QNAN]).ex(0),
        ]);
    }

    #[test]
    fn fprem_and_fprem1() {
        let (fprem, fprem1) = ([0xD9, 0xF8], [0xD9, 0xF5]);
        let cc = C0 | C1 | C2 | C3;
        check_all(&[
            case("17 rem 5", &fprem)
                .st(&[i(17), i(5)])
                .want(&[i(2), i(5)])
                .sw(cc, C3 | C1),
            case("-17 rem 5", &fprem)
                .st(&[i(-17), i(5)])
                .want(&[i(-2), i(5)])
                .sw(cc, C3 | C1),
            case("q=7", &fprem)
                .st(&[i(36), i(5)])
                .want(&[i(1), i(5)])
                .sw(cc, C0 | C3 | C1),
            case("q=4", &fprem)
                .st(&[i(21), i(5)])
                .want(&[i(1), i(5)])
                .sw(cc, C0),
            case("q=0", &fprem)
                .st(&[i(3), i(5)])
                .want(&[i(3), i(5)])
                .sw(cc, 0),
            case("clears a stale C2", &[0xD9, 0xF8])
                .st(&[i(1), i(5)])
                .sw(C2, 0),
            case("exact", &fprem)
                .st(&[i(10), i(5)])
                .want(&[POS_ZERO, i(5)])
                .sw(cc, C3),
            case("-exact keeps the sign", &fprem)
                .st(&[i(-10), i(5)])
                .want(&[NEG_ZERO, i(5)]),
            case("partial", &fprem)
                .st(&[0x7000_8000_0000_0000_0000, i(3)])
                .sw(C2, C2),
            case("x rem 0", &fprem)
                .st(&[i(1), POS_ZERO])
                .want(&[INDEFINITE, POS_ZERO])
                .ex(SW_IE),
            case("Inf rem x", &fprem)
                .st(&[POS_INF, i(1)])
                .want(&[INDEFINITE, i(1)])
                .ex(SW_IE),
            case("x rem Inf", &fprem)
                .st(&[i(3), POS_INF])
                .want(&[i(3), POS_INF])
                .sw(cc | 0x7F, 0),
            case("0 rem x", &fprem)
                .st(&[NEG_ZERO, i(3)])
                .want(&[NEG_ZERO, i(3)])
                .ex(0),
            case("NaN", &fprem)
                .st(&[QNAN, i(3)])
                .want(&[QNAN, i(3)])
                .sw(C2, 0),
            case("1 17 rem1 5", &fprem1)
                .st(&[i(17), i(5)])
                .want(&[i(2), i(5)])
                .sw(cc, C3 | C1),
            case("1 18 rem1 5", &fprem1)
                .st(&[i(18), i(5)])
                .want(&[i(-2), i(5)])
                .sw(cc, C0),
            case("1 tie to even", &fprem1)
                .st(&[TWO_POINT_FIVE, i(5)])
                .want(&[TWO_POINT_FIVE, i(5)])
                .sw(cc, 0),
            case("1 tie up", &fprem1)
                .st(&[0x4001_F000_0000_0000_0000, i(5)])
                .want(&[0xC000_A000_0000_0000_0000, i(5)])
                .sw(cc, C3),
            case("1 just over half", &fprem1)
                .st(&[i(3), i(5)])
                .want(&[i(-2), i(5)])
                .sw(cc, C1),
            case("1 partial", &fprem1)
                .st(&[0x7000_8000_0000_0000_0000, i(3)])
                .sw(C2, C2),
        ]);
        // The partial remainder loop every guest writes converges, and agrees
        // with a remainder worked out as whole numbers.
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        cpu.set_real_segment(CS, (CODE >> 4) as u16);
        cpu.fpu.push_raw(from_i64(7));
        cpu.fpu.push_raw(F80 {
            st: 0x407F_8000_0000_0000_0000,
        });
        mem.data[CODE..CODE + 2].copy_from_slice(&[0xD9, 0xF8]);
        let mut rounds = 0;
        loop {
            cpu.ip = 0;
            assert_eq!(cpu.step(&mut mem), Step::Ok);
            rounds += 1;
            if cpu.fpu.status_word() & C2 == 0 {
                break;
            }
            assert!(rounds < 100, "FPREM didn't converge");
        }
        // 2^128 mod 7 is 4.
        assert_eq!(cpu.fpu.get(0).st, i(4));
        assert!(rounds > 1);
    }

    #[test]
    fn scale_extract_and_stack_pointer() {
        let f = [0xD9, 0xFD];
        let x = [0xD9, 0xF4];
        check_all(&[
            case("FSCALE up", &f)
                .st(&[i(3), i(4)])
                .want(&[i(48), i(4)])
                .ex(0),
            case("FSCALE down", &f)
                .st(&[i(8), i(-2)])
                .want(&[i(2), i(-2)])
                .ex(0),
            case("FSCALE truncates", &f)
                .st(&[i(1), TWO_POINT_FIVE])
                .want(&[i(4), TWO_POINT_FIVE]),
            case("FSCALE overflow", &f)
                .st(&[i(1), i(40000)])
                .want(&[POS_INF, i(40000)])
                .ex(SW_OE | SW_PE),
            case("FSCALE underflow", &f)
                .st(&[i(3), i(-40000)])
                .want(&[POS_ZERO, i(-40000)])
                .ex(SW_UE | SW_PE),
            case("FSCALE by Inf", &f)
                .st(&[i(3), POS_INF])
                .want(&[POS_INF, POS_INF])
                .ex(0),
            case("FSCALE by -Inf", &f)
                .st(&[i(-3), NEG_INF])
                .want(&[NEG_ZERO, NEG_INF])
                .ex(0),
            case("FSCALE 0 by Inf", &f)
                .st(&[POS_ZERO, POS_INF])
                .want(&[INDEFINITE, POS_INF])
                .ex(SW_IE),
            case("FSCALE Inf by -Inf", &f)
                .st(&[POS_INF, NEG_INF])
                .want(&[INDEFINITE, NEG_INF])
                .ex(SW_IE),
            case("FXTRACT", &x)
                .st(&[i(-12)])
                .want(&[0xBFFF_C000_0000_0000_0000, i(3)])
                .ex(0),
            case("FXTRACT denormal", &x)
                .st(&[DENORMAL])
                .want(&[i(1), i(-16445)])
                .ex(SW_DE),
            case("FXTRACT 0", &x)
                .st(&[NEG_ZERO])
                .want(&[NEG_ZERO, NEG_INF])
                .ex(SW_ZE),
            case("FXTRACT Inf", &x)
                .st(&[NEG_INF])
                .want(&[NEG_INF, POS_INF])
                .ex(0),
            case("FXTRACT NaN", &x)
                .st(&[QNAN])
                .want(&[QNAN, QNAN])
                .ex(0),
            case("FCHS", &[0xD9, 0xE0]).st(&[i(2)]).want(&[i(-2)]),
            case("FCHS -0", &[0xD9, 0xE0])
                .st(&[POS_ZERO])
                .want(&[NEG_ZERO]),
            case("FABS", &[0xD9, 0xE1]).st(&[i(-2)]).want(&[i(2)]),
            case("FABS NaN", &[0xD9, 0xE1])
                .st(&[QNAN | (1 << 79)])
                .want(&[QNAN])
                .ex(0),
            case("FNOP", &[0xD9, 0xD0])
                .st(&[i(2)])
                .want(&[i(2)])
                .sw(FLAGS_ONLY, 0),
        ]);
        // FDECSTP and FINCSTP move TOP and leave registers and tags.
        let (mut cpu, mut mem) = setup(&case("FDECSTP", &[0xD9, 0xF6]).st(&[i(1)]));
        let tags = cpu.fpu.tags;
        cpu.step(&mut mem);
        assert_eq!(cpu.fpu.top, 6);
        assert_eq!(cpu.fpu.tags, tags);
        assert!(cpu.fpu.is_empty(0));
        assert_eq!(cpu.fpu.get(1).st, i(1));
        let (mut cpu, mut mem) = setup(&case("FINCSTP", &[0xD9, 0xF7]).st(&[i(1)]));
        cpu.step(&mut mem);
        assert_eq!(cpu.fpu.top, 0);
        assert!(!cpu.fpu.is_empty(7));
        // FFREE empties without moving TOP.
        let (mut cpu, mut mem) = setup(&case("FFREE", &[0xDD, 0xC1]).st(&[i(1), i(2)]));
        cpu.step(&mut mem);
        assert_eq!(cpu.fpu.top, 6);
        assert!(!cpu.fpu.is_empty(0));
        assert!(cpu.fpu.is_empty(1));
        // Tag values follow the contents.
        let (mut cpu, mut mem) =
            setup(&case("tags", &[0xD9, 0xD0]).st(&[DENORMAL, POS_ZERO, i(1)]));
        cpu.step(&mut mem);
        assert_eq!(cpu.fpu.tags, 0b_00_01_10_11_11_11_11_11);
    }

    #[test]
    fn transcendentals_are_accurate() {
        let (fsin, fcos, ftan) = ([0xD9, 0xFE], [0xD9, 0xFF], [0xD9, 0xF2]);
        let fsincos = [0xD9, 0xFB];
        // (argument, sine, cosine, tangent), worked out with exact integers.
        let table: [(u128, u128, u128, u128); 7] = [
            (
                0x3FFF_8000_0000_0000_0000,
                0x3FFE_D76A_A478_4867_7021,
                0x3FFE_8A51_407D_A834_5C92,
                0x3FFF_C759_22E5_F71D_2DC5,
            ),
            (
                0x3FFE_8000_0000_0000_0000,
                0x3FFD_F577_43A2_582F_7F44,
                0x3FFE_E0A9_4032_DBEA_7CEE,
                0x3FFE_8BDA_7ADF_9A3A_5219,
            ),
            (
                0x4008_FA00_0000_0000_0000,
                0x3FFE_D3AE_60A8_5103_5AC9,
                0x3FFE_8FF8_133C_9F8D_DBB8,
                0x3FFF_BC33_94F9_A188_CF7A,
            ),
            (
                0x4027_8000_0000_0000_0000,
                0xBFFD_CFB8_9138_B6AE_C68E,
                0xBFFE_E9FC_2BBD_9D54_41D9,
                0x3FFD_E343_CAE2_A491_A1A0,
            ),
            (
                0x403D_8000_0000_0000_0002,
                0xBFFE_FA71_EADB_EAD1_F984,
                0x3FFC_D429_9770_D4C2_C9E9,
                0xC001_9718_A58D_A8B5_DC03,
            ),
            (
                0xC000_C000_0000_0000_0000,
                0xBFFC_9081_C36D_B6AA_DA79,
                0xBFFE_FD70_25F4_2F2E_9308,
                0x3FFC_91F7_B892_A5C3_7866,
            ),
            // Next to pi, where most of the bits cancel.
            (
                0x4000_C90F_DAA2_2168_C235,
                0xBFBE_ECE6_75D1_FC8F_8CBB,
                0xBFFF_8000_0000_0000_0000,
                0x3FBE_ECE6_75D1_FC8F_8CBB,
            ),
        ];
        let mut rows = Vec::new();
        for (x, s, c, t) in table {
            rows.push(case("FSIN", &fsin).st(&[x]).want(&[s]).near(1).sw(C2, 0));
            rows.push(case("FCOS", &fcos).st(&[x]).want(&[c]).near(1).sw(C2, 0));
            rows.push(
                case("FPTAN", &ftan)
                    .st(&[x])
                    .want(&[i(1), t])
                    .near(1)
                    .sw(C2, 0),
            );
            rows.push(
                case("FSINCOS", &fsincos)
                    .st(&[x])
                    .want(&[c, s])
                    .near(1)
                    .sw(C2, 0),
            );
        }
        check_all(&rows);
        let (f2xm1, fyl2x, fyl2xp1, fpatan) =
            ([0xD9, 0xF0], [0xD9, 0xF1], [0xD9, 0xF9], [0xD9, 0xF3]);
        let tenth = 0x3FFB_CCCC_CCCC_CCCC_CCCD;
        let e_minus5 = 0x3FEE_A7C5_AC47_1B47_8423;
        let e_minus3 = 0x3FF5_8312_6E97_8D4F_DF3B;
        check_all(&[
            case("F2XM1 0.5", &f2xm1)
                .st(&[HALF])
                .want(&[0x3FFD_D413_CCCF_E779_9211])
                .near(1)
                .ex(SW_PE),
            case("F2XM1 -0.25", &f2xm1)
                .st(&[0xBFFD_8000_0000_0000_0000])
                .want(&[0xBFFC_A2EC_0CD4_A58A_542F])
                .near(1),
            case("F2XM1 tiny", &f2xm1)
                .st(&[e_minus5])
                .want(&[0x3FED_E895_1885_AF47_9379])
                .near(2),
            case("F2XM1 1", &f2xm1).st(&[i(1)]).want(&[i(1)]),
            case("F2XM1 -1", &f2xm1)
                .st(&[i(-1)])
                .want(&[0xBFFE_8000_0000_0000_0000]),
            case("F2XM1 0", &f2xm1)
                .st(&[NEG_ZERO])
                .want(&[NEG_ZERO])
                .ex(0),
            case("F2XM1 NaN", &f2xm1)
                .st(&[SNAN])
                .want(&[SNAN | (1 << 62)])
                .ex(SW_IE),
            case("FYL2X log2 3", &fyl2x)
                .st(&[i(3), i(1)])
                .want(&[0x3FFF_CAE0_0D1C_FDEB_43D0])
                .near(1),
            case("FYL2X 2 log2 10", &fyl2x)
                .st(&[i(10), i(2)])
                .want(&[0x4001_D49A_784B_CD1B_8AFE])
                .near(1),
            case("FYL2X log2 0.1", &fyl2x)
                .st(&[tenth, i(1)])
                .want(&[0xC000_D49A_784B_CD1B_8AFE])
                .near(2),
            case("FYL2X power", &fyl2x).st(&[i(8), i(5)]).want(&[i(15)]),
            case("FYL2X of 1", &fyl2x)
                .st(&[i(1), i(5)])
                .want(&[POS_ZERO])
                .ex(0),
            case("FYL2X of 0", &fyl2x)
                .st(&[POS_ZERO, i(5)])
                .want(&[NEG_INF])
                .ex(SW_ZE),
            case("FYL2X of -1", &fyl2x)
                .st(&[i(-1), i(5)])
                .want(&[INDEFINITE])
                .ex(SW_IE),
            case("FYL2X of Inf", &fyl2x)
                .st(&[POS_INF, i(5)])
                .want(&[POS_INF])
                .ex(0),
            case("FYL2XP1", &fyl2xp1)
                .st(&[e_minus5, i(3)])
                .want(&[0x3FF0_B588_3A4F_C44B_B4A9])
                .near(2),
            case("FYL2XP1 -0.25", &fyl2xp1)
                .st(&[0xBFFD_8000_0000_0000_0000, i(1)])
                .want(&[0xBFFD_D47F_CB8C_0852_F0C1])
                .near(1),
            case("FYL2XP1 0", &fyl2xp1)
                .st(&[POS_ZERO, i(3)])
                .want(&[POS_ZERO])
                .ex(0),
            case("FPATAN 1,1", &fpatan)
                .st(&[i(1), i(1)])
                .want(&[0x3FFE_C90F_DAA2_2168_C235])
                .near(1)
                .ex(SW_PE),
            case("FPATAN 1,3", &fpatan)
                .st(&[i(3), i(1)])
                .want(&[0x3FFD_A4BC_7D19_34F7_0924])
                .near(1),
            case("FPATAN 3,-4", &fpatan)
                .st(&[i(-4), i(3)])
                .want(&[0x4000_9FE0_BB5B_D42A_FFEC])
                .near(1),
            case("FPATAN small", &fpatan)
                .st(&[i(1), e_minus3 | (1 << 79)])
                .want(&[0xBFF5_8312_6BBA_8B70_AB74])
                .near(2),
            case("FPATAN 7,0.5", &fpatan)
                .st(&[HALF, i(7)])
                .want(&[0x3FFF_BFEF_4043_8614_FFF8])
                .near(1),
            case("FPATAN 0,1", &fpatan)
                .st(&[i(1), NEG_ZERO])
                .want(&[NEG_ZERO])
                .ex(0),
            case("FPATAN 0,-1", &fpatan)
                .st(&[i(-1), POS_ZERO])
                .want(&[0x4000_C90F_DAA2_2168_C235])
                .near(1),
            case("FPATAN 1,0", &fpatan)
                .st(&[POS_ZERO, i(1)])
                .want(&[0x3FFF_C90F_DAA2_2168_C235])
                .near(1),
            case("FPATAN Inf,Inf", &fpatan)
                .st(&[POS_INF, POS_INF])
                .want(&[0x3FFE_C90F_DAA2_2168_C235])
                .near(1),
            case("FPATAN NaN", &fpatan)
                .st(&[QNAN, i(1)])
                .want(&[QNAN])
                .ex(0),
        ]);
    }

    #[test]
    fn trigonometric_range_limit() {
        let big = 0x403E_8000_0000_0000_0000;
        let huge = 0x7000_8000_0000_0000_0000;
        let just_below = 0x403D_FFFF_FFFF_FFFF_FFFF;
        let rows = [
            ("FSIN", 0xFEu8),
            ("FCOS", 0xFF),
            ("FPTAN", 0xF2),
            ("FSINCOS", 0xFB),
        ];
        for (name, modrm) in rows {
            let code = [0xD9, modrm];
            check_all(&[
                case(name, &code).st(&[big]).want(&[big]).sw(C2 | 0x7F, C2),
                case(name, &code)
                    .st(&[huge])
                    .want(&[huge])
                    .sw(C2 | 0x7F, C2),
                case(name, &code).st(&[just_below]).sw(C2, 0),
                case(name, &code).st(&[POS_INF]).sw(C2 | 0x7F, SW_IE),
            ]);
        }
    }

    /// Runs a sequence that ends with HLT.
    fn run(cpu: &mut Cpu, mem: &mut Mem, bytes: &[u8]) {
        cpu.set_real_segment(CS, (CODE >> 4) as u16);
        cpu.set_real_segment(DS, 0);
        cpu.ip = 0;
        cpu.halted = false;
        mem.data[CODE..CODE + bytes.len()].copy_from_slice(bytes);
        for _ in 0..bytes.len() {
            if cpu.step(mem) != Step::Ok {
                break;
            }
        }
    }

    #[test]
    fn saves_are_exactly_sized_and_restore() {
        const GUARD: u8 = 0xA5;
        // (name, code, size).
        let rows: [(&str, &[u8], usize); 4] = [
            ("FNSTENV 16", &[0xD9, 0x36, 0x00, 0x02], 14),
            ("FNSTENV 32", &[0x66, 0xD9, 0x36, 0x00, 0x02], 28),
            ("FNSAVE 16", &[0xDD, 0x36, 0x00, 0x02], 94),
            ("FNSAVE 32", &[0x66, 0xDD, 0x36, 0x00, 0x02], 108),
        ];
        for (name, code, size) in rows {
            for protected in [false, true] {
                let mut cpu = Cpu::new();
                let mut mem = Mem::new();
                for byte in mem.data[OPERAND..OPERAND + 200].iter_mut() {
                    *byte = GUARD;
                }
                cpu.set_real_segment(CS, (CODE >> 4) as u16);
                cpu.set_real_segment(DS, 0);
                if protected {
                    // MOV CR0, EAX. The segments' hidden parts stay valid.
                    cpu.r[crate::cpu::AX] = cpu.cr0 | 1;
                    mem.data[CODE..CODE + 3].copy_from_slice(&[0x0F, 0x22, 0xC0]);
                    assert_eq!(cpu.step(&mut mem), Step::Ok, "{name}");
                    assert!(cpu.protected_mode());
                }
                cpu.fpu.push_raw(from_i64(1));
                cpu.fpu.push_raw(from_i64(2));
                let start = cpu.ip as usize;
                mem.data[CODE + start..CODE + start + code.len()].copy_from_slice(code);
                assert_eq!(cpu.step(&mut mem), Step::Ok, "{name}");
                assert_eq!(mem.data[OPERAND + size], GUARD, "{name}: overran");
                assert_eq!(mem.data[OPERAND + size + 1], GUARD, "{name}: overran");
                assert_ne!(mem.data[OPERAND + size - 2], GUARD, "{name}: too short");
                assert_eq!(
                    u16::from_le_bytes([mem.data[OPERAND], mem.data[OPERAND + 1]]),
                    0x037F,
                    "{name}: control"
                );
            }
        }
    }

    #[test]
    fn fnstenv_layout_real_16() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        mem.data[0x400..0x40A].copy_from_slice(&from_i64(5).get_bytes());
        run(
            &mut cpu,
            &mut mem,
            &[
                0xD9, 0xE8, // FLD1
                0xDB, 0x2E, 0x00, 0x04, // FLD tbyte [0400]
                0xD9, 0x36, 0x00, 0x02, // FNSTENV [0200]
                0xF4,
            ],
        );
        let word =
            |at: usize| u16::from_le_bytes([mem.data[OPERAND + at], mem.data[OPERAND + at + 1]]);
        assert_eq!(word(0), 0x037F);
        assert_eq!((word(2) >> 11) & 7, 6, "TOP");
        assert_eq!(word(4), 0b_00_00_11_11_11_11_11_11, "tags");
        // The data pointer is the linear address of the FLD operand.
        assert_eq!(word(10), 0x0400);
        assert_eq!(word(12) >> 12, 0);
        // The opcode is DB, the reg field 5 and the rm field 6.
        assert_eq!(word(8) & 0x7FF, 0x32E);
        // FNSTENV masks exceptions.
        assert_eq!(cpu.fpu.control & 0x3F, 0x3F);
    }

    #[test]
    fn fnsave_frstor_round_trip() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        for wide in [false, true] {
            let prefix: &[u8] = if wide { &[0x66] } else { &[] };
            let mut code = Vec::new();
            code.extend_from_slice(&[0xD9, 0xE8, 0xD9, 0xEB, 0xD9, 0xEE]); // FLD1, FLDPI, FLDZ
            code.extend_from_slice(prefix);
            code.extend_from_slice(&[0xDD, 0x36, 0x00, 0x02]); // FNSAVE
            cpu.fpu.init();
            cpu.fpu.control = 0x0B7F;
            cpu.fpu.status = C3 | SW_PE;
            cpu.set_real_segment(CS, (CODE >> 4) as u16);
            cpu.set_real_segment(DS, 0);
            cpu.ip = 0;
            mem.data[CODE..CODE + code.len()].copy_from_slice(&code);
            for _ in 0..4 {
                assert_eq!(cpu.step(&mut mem), Step::Ok);
            }
            // FNSAVE reinitializes.
            assert_eq!(cpu.fpu.control, DEFAULT_CONTROL);
            assert_eq!(cpu.fpu.tags, 0xFFFF);
            assert_eq!(cpu.fpu.top, 0);
            let mut restore = Vec::new();
            restore.extend_from_slice(prefix);
            restore.extend_from_slice(&[0xDD, 0x26, 0x00, 0x02]); // FRSTOR
            cpu.ip = 0;
            mem.data[CODE..CODE + restore.len()].copy_from_slice(&restore);
            assert_eq!(cpu.step(&mut mem), Step::Ok);
            assert_eq!(cpu.fpu.control, 0x0B7F);
            assert_eq!(cpu.fpu.top, 5);
            assert_eq!(cpu.fpu.status_word() & (C3 | SW_PE), C3 | SW_PE);
            assert_eq!(cpu.fpu.get(0).st, POS_ZERO);
            assert_eq!(cpu.fpu.get(1).st, 0x4000_C90F_DAA2_2168_C235);
            assert_eq!(cpu.fpu.get(2).st, i(1));
            assert!(cpu.fpu.is_empty(3));
            assert_eq!(cpu.fpu.tag_word() >> 10, 0b00_00_01);
        }
    }

    #[test]
    fn fldenv_restores_what_fnstenv_stored() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        cpu.fpu.push_raw(from_i64(9));
        cpu.fpu.control = 0x0A7B;
        cpu.fpu.status = C0 | SW_ZE;
        run(
            &mut cpu,
            &mut mem,
            &[
                0xD9, 0x36, 0x00, 0x02, // FNSTENV
                0xDB, 0xE3, // FNINIT
                0xD9, 0x26, 0x00, 0x02, // FLDENV
                0xF4,
            ],
        );
        assert_eq!(cpu.fpu.control, 0x0A7B);
        assert_eq!(cpu.fpu.top, 7);
        assert!(!cpu.fpu.is_empty(0));
        assert_eq!(cpu.fpu.status_word() & (C0 | SW_ZE), C0 | SW_ZE);
    }

    #[test]
    fn control_and_status_words() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        mem.data[0x300..0x302].copy_from_slice(&0x0C72u16.to_le_bytes());
        run(
            &mut cpu,
            &mut mem,
            &[
                0xD9, 0x2E, 0x00, 0x03, // FLDCW [0300]
                0xD9, 0x3E, 0x02, 0x03, // FNSTCW [0302]
                0xD9, 0xE8, // FLD1
                0xDF, 0xE0, // FNSTSW AX
                0xDD, 0x3E, 0x04, 0x03, // FNSTSW [0304]
                0xF4,
            ],
        );
        assert_eq!(
            u16::from_le_bytes([mem.data[0x302], mem.data[0x303]]),
            0x0C72
        );
        assert_eq!((cpu.r[AX] >> 11) & 7, 7);
        assert_eq!(
            cpu.r[AX] as u16,
            u16::from_le_bytes([mem.data[0x304], mem.data[0x305]])
        );
        // RC from FLDCW takes effect, and FNINIT resets.
        assert_eq!(cpu.fpu.rc(), RC_CHOP);
        run(&mut cpu, &mut mem, &[0xDB, 0xE3, 0xF4]);
        assert_eq!(cpu.fpu.control, DEFAULT_CONTROL);
        assert_eq!(cpu.fpu.status_word(), 0);
        assert_eq!(cpu.fpu.tags, 0xFFFF);
    }

    #[test]
    fn unmasked_exceptions_set_summary_bits_only() {
        let zero_unmasked: u16 = 0x037B;
        check_all(&[
            // Division by zero: the flag, ES and B, and still the masked result.
            case("ZE unmasked", &[0xD8, 0xF1])
                .cw(zero_unmasked)
                .st(&[i(1), POS_ZERO])
                .want(&[POS_INF, POS_ZERO])
                .sw(SW_ZE | SW_ES | SW_B, SW_ZE | SW_ES | SW_B),
            case("ZE masked", &[0xD8, 0xF1])
                .st(&[i(1), POS_ZERO])
                .sw(SW_ZE | SW_ES | SW_B, SW_ZE),
            case("PE unmasked doesn't see ZE", &[0xD8, 0xF1])
                .cw(0x035F)
                .st(&[i(1), POS_ZERO])
                .sw(SW_ES | SW_B, 0),
        ]);
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        cpu.fpu.control = zero_unmasked;
        cpu.fpu.push_raw(F80::new());
        cpu.fpu.push_raw(from_i64(1));
        run(&mut cpu, &mut mem, &[0xD8, 0xF1, 0xDB, 0xE2, 0xF4]); // FDIV, FNCLEX
        assert_eq!(cpu.fpu.status_word() & 0x80FF, 0, "FNCLEX");
    }

    #[test]
    fn old_287_controls_do_nothing() {
        for modrm in [0xE0, 0xE1, 0xE4] {
            let row = case("287", &[0xDB, modrm])
                .st(&[i(1)])
                .want(&[i(1)])
                .sw(NOT_TOP, 0);
            check(&row);
        }
    }

    #[test]
    fn condition_moves_and_aliases() {
        check_all(&[
            case("FSTP8", &[0xDF, 0xD1]).st(&[i(1), i(2)]).want(&[i(1)]),
            case("FXCH7", &[0xDF, 0xC9])
                .st(&[i(1), i(2)])
                .want(&[i(2), i(1)]),
            case("FFREEP", &[0xDF, 0xC0])
                .st(&[i(1), i(2)])
                .want(&[i(2)]),
            case("FCOM2", &[0xDC, 0xD1])
                .st(&[i(1), i(2)])
                .want(&[i(1), i(2)])
                .sw(CC & !C1, C0),
            case("FCOMP3", &[0xDC, 0xD9])
                .st(&[i(1), i(2)])
                .want(&[i(2)])
                .sw(CC & !C1, C0),
        ]);
        // FCMOVB takes the source only when CF is set.
        for (carry, want) in [(false, i(1)), (true, i(2))] {
            let row = case("FCMOVB", &[0xDA, 0xC1]).st(&[i(1), i(2)]);
            let (mut cpu, mut mem) = setup(&row);
            cpu.set_flag(FLAG_CF, carry);
            cpu.step(&mut mem);
            assert_eq!(cpu.fpu.get(0).st, want);
        }
    }

    /// A little generator, so the fuzz tests are the same on every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn value(&mut self) -> u128 {
            let exponent = match self.next() % 8 {
                0 => 0,
                1 => 0x7FFF,
                2 => 0x7FFE,
                3 => 0x3FFF,
                4 => 1,
                5 => 0x3FFF + (self.next() % 128) as u128 - 64,
                _ => (self.next() & 0x7FFF) as u128,
            };
            let mantissa = match self.next() % 6 {
                0 => 0,
                1 => 1 << 63,
                2 => u64::MAX,
                3 => self.next() >> 1,
                _ => self.next() | (1 << 63),
            };
            ((self.next() as u128 & 1) << 79) | (exponent << 64) | mantissa as u128
        }
    }

    #[test]
    fn garbage_in_every_encoding_never_panics() {
        let mut rng = Rng(0x0123_4567_89AB_CDEF);
        let mut mem = Mem::new();
        for byte in mem.data.iter_mut().take(0x1_0000) {
            *byte = rng.next() as u8;
        }
        for pass in 0..32 {
            for opcode in 0xD8..=0xDFu8 {
                for modrm in 0..=255u8 {
                    let mut cpu = Cpu::new();
                    for slot in 0..8 {
                        cpu.fpu.st[slot] = F80 { st: rng.value() };
                    }
                    cpu.fpu.top = (rng.next() & 7) as u8;
                    cpu.fpu.tags = rng.next() as u16;
                    cpu.fpu.status = rng.next() as u16;
                    cpu.fpu.control = rng.next() as u16;
                    cpu.r = [rng.next() as u32 & 0xFFFF; 8];
                    cpu.set_real_segment(CS, (CODE >> 4) as u16);
                    cpu.ip = 0;
                    let mut code = [0u8; 12];
                    let mut used = 0;
                    if pass & 1 != 0 {
                        code[used] = 0x66;
                        used += 1;
                    }
                    if pass & 2 != 0 {
                        code[used] = 0x67;
                        used += 1;
                    }
                    code[used] = opcode;
                    code[used + 1] = modrm;
                    for byte in code[used + 2..].iter_mut() {
                        *byte = rng.next() as u8;
                    }
                    mem.data[CODE..CODE + 12].copy_from_slice(&code);
                    // Faults are fine. Panics aren't.
                    let _ = cpu.step(&mut mem);
                    assert!(cpu.fpu.top < 8);
                    let _ = cpu.fpu.status_word();
                }
            }
        }
    }

    #[test]
    fn a_fault_leaves_the_stack_alone() {
        // A store past the 64K limit in 32-bit addressing faults, and doesn't pop.
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        cpu.set_real_segment(CS, (CODE >> 4) as u16);
        cpu.set_real_segment(DS, 0);
        cpu.seg[DS] = Seg {
            hi: 0x0FFF,
            ..cpu.seg[DS]
        };
        cpu.fpu.push_raw(from_i64(1));
        cpu.r[crate::cpu::BX] = 0x0FFE;
        // FSTP dword [EBX] with an address size prefix. Its last byte is over the limit.
        mem.data[CODE..CODE + 3].copy_from_slice(&[0x67, 0xD9, 0x1B]);
        let _ = cpu.step(&mut mem);
        assert_eq!(cpu.fpu.top, 7);
        assert!(!cpu.fpu.is_empty(0));
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
        assert_eq!((cpu.r[AX] >> 11) & 7, 7);
    }
}
