//! An 8086/80186 interpreter.
//!
//! The CPU is deliberately small and explicit: every instruction is decoded
//! and executed by [`Cpu::step`], which is enough for a text-mode DOS machine
//! and keeps the core easy to audit. It implements the complete 8086 integer
//! instruction set plus the 80186 conveniences (`PUSHA`, `ENTER`, immediate
//! shifts, `IMUL` with an immediate) that DOS programs commonly use.
//!
//! Memory, ports and interrupts are supplied by the [`Bus`] trait, so the same
//! CPU runs against the FEOS-DOS machine, a host-test memory or a future
//! debugger.

/// Register indices in [`Cpu::r`].
pub const AX: usize = 0;
pub const CX: usize = 1;
pub const DX: usize = 2;
pub const BX: usize = 3;
pub const SP: usize = 4;
pub const BP: usize = 5;
pub const SI: usize = 6;
pub const DI: usize = 7;

/// Segment register indices in [`Cpu::sregs`].
pub const ES: usize = 0;
pub const CS: usize = 1;
pub const SS: usize = 2;
pub const DS: usize = 3;

/// Carry flag.
pub const FLAG_CF: u16 = 0x0001;
/// Parity flag.
pub const FLAG_PF: u16 = 0x0004;
/// Auxiliary carry flag.
pub const FLAG_AF: u16 = 0x0010;
/// Zero flag.
pub const FLAG_ZF: u16 = 0x0040;
/// Sign flag.
pub const FLAG_SF: u16 = 0x0080;
/// Trap flag.
pub const FLAG_TF: u16 = 0x0100;
/// Interrupt-enable flag.
pub const FLAG_IF: u16 = 0x0200;
/// Direction flag.
pub const FLAG_DF: u16 = 0x0400;
/// Overflow flag.
pub const FLAG_OF: u16 = 0x0800;

/// What [`Cpu::step`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// One instruction retired.
    Ok,
    /// The instruction needs input that is not available yet. The program
    /// counter, stack pointer and flags have been restored, so the caller can
    /// retry the instruction on the next frame.
    Blocked,
    /// `HLT` was executed.
    Halted,
    /// The CPU met an instruction it cannot execute. The byte is returned so
    /// the caller can show a useful diagnostic.
    Fault(u8),
}

/// What an interrupt handler wants the CPU to do next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InterruptAction {
    /// The service completed; continue with the next instruction.
    Continue,
    /// The service needs input. Retry the `INT` instruction later.
    Blocked,
    /// The service terminated the program (`INT 20h`/`INT 21h` `4Ch`).
    Halt,
}

/// The register file as seen by an interrupt handler.
///
/// Handlers may read and write the general registers, segment registers and
/// flags. The instruction pointer is owned by the CPU.
pub struct CpuRegs<'a> {
    pub r: &'a mut [u16; 8],
    pub sregs: &'a mut [u16; 4],
    pub flags: &'a mut u16,
}

impl CpuRegs<'_> {
    pub fn ax(&self) -> u16 {
        self.r[AX]
    }
    pub fn bx(&self) -> u16 {
        self.r[BX]
    }
    pub fn cx(&self) -> u16 {
        self.r[CX]
    }
    pub fn dx(&self) -> u16 {
        self.r[DX]
    }
    pub fn si(&self) -> u16 {
        self.r[SI]
    }
    pub fn di(&self) -> u16 {
        self.r[DI]
    }
    pub fn bp(&self) -> u16 {
        self.r[BP]
    }
    pub fn sp(&self) -> u16 {
        self.r[SP]
    }
    pub fn al(&self) -> u8 {
        self.r[AX] as u8
    }
    pub fn ah(&self) -> u8 {
        (self.r[AX] >> 8) as u8
    }
    pub fn bl(&self) -> u8 {
        self.r[BX] as u8
    }
    pub fn bh(&self) -> u8 {
        (self.r[BX] >> 8) as u8
    }
    pub fn cl(&self) -> u8 {
        self.r[CX] as u8
    }
    pub fn ch(&self) -> u8 {
        (self.r[CX] >> 8) as u8
    }
    pub fn dl(&self) -> u8 {
        self.r[DX] as u8
    }
    pub fn dh(&self) -> u8 {
        (self.r[DX] >> 8) as u8
    }

    pub fn set_ax(&mut self, value: u16) {
        self.r[AX] = value;
    }
    pub fn set_bx(&mut self, value: u16) {
        self.r[BX] = value;
    }
    pub fn set_bh(&mut self, value: u8) {
        self.r[BX] = (self.r[BX] & 0x00FF) | ((value as u16) << 8);
    }
    pub fn set_bl(&mut self, value: u8) {
        self.r[BX] = (self.r[BX] & 0xFF00) | value as u16;
    }
    pub fn set_cx(&mut self, value: u16) {
        self.r[CX] = value;
    }
    pub fn set_ch(&mut self, value: u8) {
        self.r[CX] = (self.r[CX] & 0x00FF) | ((value as u16) << 8);
    }
    pub fn set_cl(&mut self, value: u8) {
        self.r[CX] = (self.r[CX] & 0xFF00) | value as u16;
    }
    pub fn set_dx(&mut self, value: u16) {
        self.r[DX] = value;
    }
    pub fn set_al(&mut self, value: u8) {
        self.r[AX] = (self.r[AX] & 0xFF00) | value as u16;
    }
    pub fn set_ah(&mut self, value: u8) {
        self.r[AX] = (self.r[AX] & 0x00FF) | ((value as u16) << 8);
    }
    pub fn set_dl(&mut self, value: u8) {
        self.r[DX] = (self.r[DX] & 0xFF00) | value as u16;
    }
    pub fn set_dh(&mut self, value: u8) {
        self.r[DX] = (self.r[DX] & 0x00FF) | ((value as u16) << 8);
    }

    pub fn ds(&self) -> u16 {
        self.sregs[DS]
    }
    pub fn es(&self) -> u16 {
        self.sregs[ES]
    }
    pub fn cs(&self) -> u16 {
        self.sregs[CS]
    }
    pub fn ss(&self) -> u16 {
        self.sregs[SS]
    }

    pub fn set_ds(&mut self, value: u16) {
        self.sregs[DS] = value;
    }
    pub fn set_es(&mut self, value: u16) {
        self.sregs[ES] = value;
    }

    pub fn carry(&self) -> bool {
        *self.flags & FLAG_CF != 0
    }
    pub fn zero(&self) -> bool {
        *self.flags & FLAG_ZF != 0
    }
    pub fn set_carry(&mut self, on: bool) {
        if on {
            *self.flags |= FLAG_CF;
        } else {
            *self.flags &= !FLAG_CF;
        }
    }
    pub fn set_zero(&mut self, on: bool) {
        if on {
            *self.flags |= FLAG_ZF;
        } else {
            *self.flags &= !FLAG_ZF;
        }
    }
}

/// Devices the CPU can reach.
pub trait Bus {
    fn read8(&mut self, addr: u32) -> u8;
    fn write8(&mut self, addr: u32, value: u8);

    fn read16(&mut self, addr: u32) -> u16 {
        self.read8(addr) as u16 | ((self.read8(addr.wrapping_add(1)) as u16) << 8)
    }

    fn write16(&mut self, addr: u32, value: u16) {
        self.write8(addr, value as u8);
        self.write8(addr.wrapping_add(1), (value >> 8) as u8);
    }

    fn in8(&mut self, _port: u16) -> u8 {
        0xFF
    }

    fn out8(&mut self, _port: u16, _value: u8) {}

    fn in16(&mut self, port: u16) -> u16 {
        self.in8(port) as u16 | ((self.in8(port.wrapping_add(1)) as u16) << 8)
    }

    fn out16(&mut self, port: u16, value: u16) {
        self.out8(port, value as u8);
        self.out8(port.wrapping_add(1), (value >> 8) as u8);
    }

    /// Services a software or hardware interrupt.
    fn interrupt(&mut self, _vector: u8, _regs: &mut CpuRegs<'_>) -> InterruptAction {
        InterruptAction::Continue
    }
}

/// One entry in the CPU's instruction trace, kept for fault diagnostics.
#[derive(Clone, Copy, Default)]
pub struct TraceEntry {
    pub cs: u16,
    pub ip: u16,
    pub opcode: u8,
    pub ax: u16,
    pub bx: u16,
    pub cx: u16,
    pub dx: u16,
    pub sp: u16,
    pub bp: u16,
    pub si: u16,
    pub di: u16,
    pub ds: u16,
    pub es: u16,
    pub flags: u16,
}

/// Number of instructions kept in the trace ring.
pub const TRACE_LEN: usize = 32;

/// The processor state.
#[derive(Clone)]
pub struct Cpu {
    /// `AX, CX, DX, BX, SP, BP, SI, DI`.
    pub r: [u16; 8],
    /// `ES, CS, SS, DS`.
    pub sregs: [u16; 4],
    pub ip: u16,
    pub flags: u16,
    pub halted: bool,
    /// Instructions retired since reset.
    pub cycles: u64,
    /// Prefix state for the instruction being decoded.
    seg_override: Option<u16>,
    rep: Option<u8>,
    /// Ring of recently executed instructions, for fault reports.
    pub trace: [TraceEntry; TRACE_LEN],
    pub trace_index: usize,
    /// x87 floating-point state.
    pub fpu: crate::fpu::Fpu,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            r: [0; 8],
            sregs: [0; 4],
            ip: 0,
            flags: 0x0202,
            halted: false,
            cycles: 0,
            seg_override: None,
            rep: None,
            trace: [TraceEntry::default(); TRACE_LEN],
            trace_index: 0,
            fpu: crate::fpu::Fpu::new(),
        }
    }

    /// Resets the register file to the state the BIOS leaves at boot.
    pub fn reset(&mut self) {
        self.r = [0; 8];
        self.sregs = [0; 4];
        self.ip = 0;
        self.flags = 0x0202;
        self.halted = false;
        self.cycles = 0;
        self.seg_override = None;
        self.rep = None;
        self.trace = [TraceEntry::default(); TRACE_LEN];
        self.trace_index = 0;
        self.fpu = crate::fpu::Fpu::new();
    }

    pub const fn physical(segment: u16, offset: u16) -> u32 {
        ((segment as u32) << 4).wrapping_add(offset as u32) & 0x000F_FFFF
    }

    pub fn reg8(&self, index: u8) -> u8 {
        let index = (index & 7) as usize;
        if index < 4 {
            self.r[index] as u8
        } else {
            (self.r[index - 4] >> 8) as u8
        }
    }

    pub fn set_reg8(&mut self, index: u8, value: u8) {
        let index = (index & 7) as usize;
        if index < 4 {
            self.r[index] = (self.r[index] & 0xFF00) | value as u16;
        } else {
            self.r[index - 4] = (self.r[index - 4] & 0x00FF) | ((value as u16) << 8);
        }
    }

    fn fetch8<B: Bus>(&mut self, bus: &mut B) -> u8 {
        let value = bus.read8(Self::physical(self.sregs[CS], self.ip));
        self.ip = self.ip.wrapping_add(1);
        value
    }

    fn fetch16<B: Bus>(&mut self, bus: &mut B) -> u16 {
        let lo = self.fetch8(bus) as u16;
        let hi = self.fetch8(bus) as u16;
        lo | (hi << 8)
    }

    fn push<B: Bus>(&mut self, bus: &mut B, value: u16) {
        self.r[SP] = self.r[SP].wrapping_sub(2);
        bus.write16(Self::physical(self.sregs[SS], self.r[SP]), value);
    }

    fn pop<B: Bus>(&mut self, bus: &mut B) -> u16 {
        let value = bus.read16(Self::physical(self.sregs[SS], self.r[SP]));
        self.r[SP] = self.r[SP].wrapping_add(2);
        value
    }

    /// Executes one instruction.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> Step {
        if self.halted {
            return Step::Halted;
        }

        let start_ip = self.ip;
        let start_sp = self.r[SP];
        let start_flags = self.flags;

        self.seg_override = None;
        self.rep = None;
        let opcode = loop {
            let op = self.fetch8(bus);
            match op {
                0x26 => self.seg_override = Some(ES as u16),
                0x2E => self.seg_override = Some(CS as u16),
                0x36 => self.seg_override = Some(SS as u16),
                0x3E => self.seg_override = Some(DS as u16),
                0xF0 => {}
                0xF2 | 0xF3 => self.rep = Some(op),
                _ => break op,
            }
        };

        let slot = self.trace_index % TRACE_LEN;
        self.trace[slot] = TraceEntry {
            cs: self.sregs[CS],
            ip: start_ip,
            opcode,
            ax: self.r[AX],
            bx: self.r[BX],
            cx: self.r[CX],
            dx: self.r[DX],
            sp: self.r[SP],
            bp: self.r[BP],
            si: self.r[SI],
            di: self.r[DI],
            ds: self.sregs[DS],
            es: self.sregs[ES],
            flags: self.flags,
        };
        self.trace_index = self.trace_index.wrapping_add(1);
        self.cycles = self.cycles.wrapping_add(1);
        let result = self.execute(bus, opcode);
        if result == Step::Blocked {
            self.ip = start_ip;
            self.r[SP] = start_sp;
            self.flags = start_flags;
        }
        result
    }

    /// Enters an interrupt handler without an instruction fetch. Used by the
    /// machine's timer to deliver `INT 1Ch` between instructions.
    pub fn inject_interrupt<B: Bus>(&mut self, bus: &mut B, vector: u8) -> Step {
        if self.flags & FLAG_IF == 0 {
            return Step::Ok;
        }
        self.software_interrupt(bus, vector)
    }

    fn software_interrupt<B: Bus>(&mut self, bus: &mut B, vector: u8) -> Step {
        let return_ip = self.ip;
        let return_cs = self.sregs[CS];
        let return_flags = self.flags;
        self.push(bus, return_flags);
        self.push(bus, return_cs);
        self.push(bus, return_ip);
        self.flags &= !(FLAG_IF | FLAG_TF);

        let action = {
            let mut regs = CpuRegs {
                r: &mut self.r,
                sregs: &mut self.sregs,
                flags: &mut self.flags,
            };
            bus.interrupt(vector, &mut regs)
        };

        match action {
            InterruptAction::Continue => Step::Ok,
            InterruptAction::Blocked => Step::Blocked,
            InterruptAction::Halt => Step::Halted,
        }
    }

    fn set_szp(&mut self, value: u16, width: u8) {
        let mask = if width == 8 { 0x00FF } else { 0xFFFF };
        let sign = if width == 8 { 0x0080 } else { 0x8000 };
        self.flags &= !(FLAG_ZF | FLAG_SF | FLAG_PF);
        let value = value & mask;
        if value == 0 {
            self.flags |= FLAG_ZF;
        }
        if value & sign != 0 {
            self.flags |= FLAG_SF;
        }
        if (value as u8).count_ones() % 2 == 0 {
            self.flags |= FLAG_PF;
        }
    }

    fn alu(&mut self, op: u8, a: u16, b: u16, width: u8) -> u16 {
        let mask: u32 = if width == 8 { 0x00FF } else { 0xFFFF };
        let sign: u16 = if width == 8 { 0x0080 } else { 0x8000 };
        let carry_in = u32::from(self.flags & FLAG_CF != 0);
        let a32 = u32::from(a) & mask;
        let b32 = u32::from(b) & mask;

        let (result, carry, overflow) = match op {
            0 | 2 => {
                let sum = a32 + b32 + if op == 2 { carry_in } else { 0 };
                let result = (sum & mask) as u16;
                let overflow = ((!(a ^ b) & (a ^ result)) & sign) != 0;
                (result, sum > mask, overflow)
            }
            3 | 5 | 7 => {
                let sub = b32 + if op == 3 { carry_in } else { 0 };
                let result = (a32.wrapping_sub(sub) & mask) as u16;
                let overflow = (((a ^ b) & (a ^ result)) & sign) != 0;
                (result, a32 < sub, overflow)
            }
            1 | 4 | 6 => {
                let result = match op {
                    1 => a | b,
                    4 => a & b,
                    _ => a ^ b,
                } & (mask as u16);
                (result, false, false)
            }
            _ => (a, false, false),
        };

        self.flags &= !(FLAG_CF | FLAG_OF | FLAG_AF);
        if carry {
            self.flags |= FLAG_CF;
        }
        if overflow {
            self.flags |= FLAG_OF;
        }
        if op <= 3 || op >= 5 {
            if op != 1 && op != 4 && op != 6 && ((a ^ b ^ result) & 0x10) != 0 {
                self.flags |= FLAG_AF;
            }
        }
        self.set_szp(result, width);
        result
    }

    fn condition(&self, code: u8) -> bool {
        let cf = self.flags & FLAG_CF != 0;
        let zf = self.flags & FLAG_ZF != 0;
        let sf = self.flags & FLAG_SF != 0;
        let of = self.flags & FLAG_OF != 0;
        let pf = self.flags & FLAG_PF != 0;
        match code & 0x0F {
            0x0 => of,
            0x1 => !of,
            0x2 => cf,
            0x3 => !cf,
            0x4 => zf,
            0x5 => !zf,
            0x6 => cf || zf,
            0x7 => !cf && !zf,
            0x8 => sf,
            0x9 => !sf,
            0xA => pf,
            0xB => !pf,
            0xC => sf != of,
            0xD => sf == of,
            0xE => zf || (sf != of),
            0xF => !zf && (sf == of),
            _ => false,
        }
    }

    fn shift(&mut self, kind: u8, value: u16, count: u8, width: u8) -> u16 {
        let mask: u16 = if width == 8 { 0x00FF } else { 0xFFFF };
        let bits = if width == 8 { 8u8 } else { 16u8 };
        let sign: u16 = if width == 8 { 0x0080 } else { 0x8000 };
        let mut count = count & 0x1F;
        let mut value = value & mask;
        if count == 0 {
            return value;
        }

        let original = value;
        let mut carry = self.flags & FLAG_CF != 0;
        match kind {
            0 => {
                count %= bits;
                for _ in 0..count {
                    let msb = value & sign != 0;
                    value = ((value << 1) | u16::from(msb)) & mask;
                    carry = msb;
                }
            }
            1 => {
                count %= bits;
                for _ in 0..count {
                    let lsb = value & 1 != 0;
                    value = (value >> 1) | (u16::from(lsb) << (bits - 1));
                    carry = lsb;
                }
            }
            2 => {
                count %= bits + 1;
                for _ in 0..count {
                    let msb = value & sign != 0;
                    value = ((value << 1) | u16::from(carry)) & mask;
                    carry = msb;
                }
            }
            3 => {
                count %= bits + 1;
                for _ in 0..count {
                    let lsb = value & 1 != 0;
                    value = (value >> 1) | (u16::from(carry) << (bits - 1));
                    carry = lsb;
                }
            }
            4 => {
                for _ in 0..count {
                    carry = value & sign != 0;
                    value = (value << 1) & mask;
                }
            }
            5 => {
                for _ in 0..count {
                    carry = value & 1 != 0;
                    value >>= 1;
                }
            }
            7 => {
                for _ in 0..count {
                    carry = value & 1 != 0;
                    value = (value >> 1) | (value & sign);
                }
            }
            _ => {}
        }

        self.flags &= !(FLAG_CF | FLAG_OF | FLAG_AF);
        if carry {
            self.flags |= FLAG_CF;
        }
        if (count & 0x1F) == 1 {
            let msb = value & sign != 0;
            let of = match kind {
                5 => original & sign != 0,
                7 => false,
                _ => msb != carry,
            };
            if of {
                self.flags |= FLAG_OF;
            }
        }
        self.set_szp(value, width);
        value & mask
    }

    fn read_rm8<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> u8 {
        if m.is_reg {
            self.reg8(m.rm)
        } else {
            bus.read8(Self::physical(m.seg, m.addr))
        }
    }

    fn write_rm8<B: Bus>(&mut self, bus: &mut B, m: &ModRm, value: u8) {
        if m.is_reg {
            self.set_reg8(m.rm, value);
        } else {
            bus.write8(Self::physical(m.seg, m.addr), value);
        }
    }

    fn read_rm16<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> u16 {
        if m.is_reg {
            self.r[m.rm as usize]
        } else {
            bus.read16(Self::physical(m.seg, m.addr))
        }
    }

    fn write_rm16<B: Bus>(&mut self, bus: &mut B, m: &ModRm, value: u16) {
        if m.is_reg {
            self.r[m.rm as usize] = value;
        } else {
            bus.write16(Self::physical(m.seg, m.addr), value);
        }
    }

    fn decode_modrm<B: Bus>(&mut self, bus: &mut B) -> ModRm {
        let byte = self.fetch8(bus);
        let mode = byte >> 6;
        let reg = (byte >> 3) & 7;
        let rm = byte & 7;
        if mode == 3 {
            return ModRm {
                reg,
                rm,
                is_reg: true,
                addr: 0,
                seg: DS as u16,
            };
        }

        let (base, default_seg) = match rm {
            0 => (self.r[BX].wrapping_add(self.r[SI]), DS),
            1 => (self.r[BX].wrapping_add(self.r[DI]), DS),
            2 => (self.r[BP].wrapping_add(self.r[SI]), SS),
            3 => (self.r[BP].wrapping_add(self.r[DI]), SS),
            4 => (self.r[SI], DS),
            5 => (self.r[DI], DS),
            6 => {
                if mode == 0 {
                    (0, DS)
                } else {
                    (self.r[BP], SS)
                }
            }
            _ => (self.r[BX], DS),
        };

        let disp = match mode {
            0 => {
                if rm == 6 {
                    self.fetch16(bus)
                } else {
                    0
                }
            }
            1 => self.fetch8(bus) as i8 as i16 as u16,
            2 => self.fetch16(bus),
            _ => 0,
        };
        let addr = base.wrapping_add(disp);
        let seg_index = self.seg_override.unwrap_or(default_seg as u16);
        let seg = self.sregs[seg_index as usize];
        ModRm {
            reg,
            rm,
            is_reg: false,
            addr,
            seg,
        }
    }

    fn execute<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Step {
        // The eight ALU groups share the 0x00..=0x3D layout.
        if opcode < 0x40 && (opcode & 7) <= 5 {
            let op = opcode >> 3;
            let form = opcode & 7;
            match form {
                0 => {
                    let m = self.decode_modrm(bus);
                    let src = self.reg8(m.reg) as u16;
                    let dst = self.read_rm8(bus, &m) as u16;
                    let result = self.alu(op, dst, src, 8);
                    if op != 7 {
                        self.write_rm8(bus, &m, result as u8);
                    }
                }
                1 => {
                    let m = self.decode_modrm(bus);
                    let src = self.r[m.reg as usize];
                    let dst = self.read_rm16(bus, &m);
                    let result = self.alu(op, dst, src, 16);
                    if op != 7 {
                        self.write_rm16(bus, &m, result);
                    }
                }
                2 => {
                    let m = self.decode_modrm(bus);
                    let src = self.read_rm8(bus, &m) as u16;
                    let dst = self.reg8(m.reg) as u16;
                    let result = self.alu(op, dst, src, 8);
                    if op != 7 {
                        self.set_reg8(m.reg, result as u8);
                    }
                }
                3 => {
                    let m = self.decode_modrm(bus);
                    let src = self.read_rm16(bus, &m);
                    let dst = self.r[m.reg as usize];
                    let result = self.alu(op, dst, src, 16);
                    if op != 7 {
                        self.r[m.reg as usize] = result;
                    }
                }
                4 => {
                    let imm = self.fetch8(bus) as u16;
                    let result = self.alu(op, self.r[AX] & 0xFF, imm, 8);
                    if op != 7 {
                        self.set_reg8(0, result as u8);
                    }
                }
                5 => {
                    let imm = self.fetch16(bus);
                    let result = self.alu(op, self.r[AX], imm, 16);
                    if op != 7 {
                        self.r[AX] = result;
                    }
                }
                _ => return Step::Fault(opcode),
            }
            return Step::Ok;
        }

        match opcode {
            0x06 => {
                let value = self.sregs[ES];
                self.push(bus, value);
            }
            0x07 => {
                let value = self.pop(bus);
                self.sregs[ES] = value;
            }
            0x0E => {
                let value = self.sregs[CS];
                self.push(bus, value);
            }
            0x0F => return Step::Fault(opcode),
            0x16 => {
                let value = self.sregs[SS];
                self.push(bus, value);
            }
            0x17 => {
                let value = self.pop(bus);
                self.sregs[SS] = value;
            }
            0x1E => {
                let value = self.sregs[DS];
                self.push(bus, value);
            }
            0x1F => {
                let value = self.pop(bus);
                self.sregs[DS] = value;
            }

            0x27 => {
                // DAA
                let mut al = self.r[AX] as u8;
                let old_al = al;
                let old_cf = self.flags & FLAG_CF != 0;
                let old_af = self.flags & FLAG_AF != 0;
                self.flags &= !(FLAG_CF | FLAG_AF);
                if (al & 0x0F) > 9 || old_af {
                    al = al.wrapping_add(6);
                    self.flags |= FLAG_AF;
                }
                if old_al > 0x99 || old_cf {
                    al = al.wrapping_add(0x60);
                    self.flags |= FLAG_CF;
                }
                self.set_reg8(0, al);
                self.set_szp(al as u16, 8);
            }
            0x2F => {
                // DAS
                let mut al = self.r[AX] as u8;
                let old_al = al;
                let old_cf = self.flags & FLAG_CF != 0;
                let old_af = self.flags & FLAG_AF != 0;
                self.flags &= !(FLAG_CF | FLAG_AF);
                if (al & 0x0F) > 9 || old_af {
                    al = al.wrapping_sub(6);
                    self.flags |= FLAG_AF;
                }
                if old_al > 0x99 || old_cf {
                    al = al.wrapping_sub(0x60);
                    self.flags |= FLAG_CF;
                }
                self.set_reg8(0, al);
                self.set_szp(al as u16, 8);
            }
            0x37 => {
                // AAA
                let mut ax = self.r[AX];
                if (ax & 0x0F) > 9 || self.flags & FLAG_AF != 0 {
                    ax = ax.wrapping_add(0x0106);
                    self.flags |= FLAG_AF | FLAG_CF;
                } else {
                    self.flags &= !(FLAG_AF | FLAG_CF);
                }
                ax &= 0xFF0F;
                self.r[AX] = ax;
            }
            0x3F => {
                // AAS
                let mut ax = self.r[AX];
                if (ax & 0x0F) > 9 || self.flags & FLAG_AF != 0 {
                    ax = ax.wrapping_sub(0x0106);
                    self.flags |= FLAG_AF | FLAG_CF;
                } else {
                    self.flags &= !(FLAG_AF | FLAG_CF);
                }
                ax &= 0xFF0F;
                self.r[AX] = ax;
            }

            0x40..=0x47 => {
                let index = (opcode & 7) as usize;
                let value = self.r[index].wrapping_add(1);
                self.r[index] = value;
                self.set_szp(value, 16);
            }
            0x48..=0x4F => {
                let index = (opcode & 7) as usize;
                let value = self.r[index].wrapping_sub(1);
                self.r[index] = value;
                self.set_szp(value, 16);
            }
            0x50..=0x57 => {
                let value = self.r[(opcode & 7) as usize];
                self.push(bus, value);
            }
            0x58..=0x5F => {
                let value = self.pop(bus);
                self.r[(opcode & 7) as usize] = value;
            }

            0x60 => {
                let sp = self.r[SP];
                for index in [AX, CX, DX, BX] {
                    let value = self.r[index];
                    self.push(bus, value);
                }
                self.push(bus, sp);
                for index in [BP, SI, DI] {
                    let value = self.r[index];
                    self.push(bus, value);
                }
            }
            0x61 => {
                for index in [DI, SI, BP] {
                    self.r[index] = self.pop(bus);
                }
                let _ = self.pop(bus);
                for index in [BX, DX, CX, AX] {
                    self.r[index] = self.pop(bus);
                }
            }
            0x68 => {
                let value = self.fetch16(bus);
                self.push(bus, value);
            }
            0x69 => {
                let m = self.decode_modrm(bus);
                let value = self.read_rm16(bus, &m);
                let imm = self.fetch16(bus);
                let result = (value as i16 as i32).wrapping_mul(imm as i16 as i32) as u16;
                self.r[m.reg as usize] = result;
                let overflow = result as i32 != (value as i16 as i32) * (imm as i16 as i32);
                self.flags &= !(FLAG_CF | FLAG_OF);
                if overflow {
                    self.flags |= FLAG_CF | FLAG_OF;
                }
            }
            0x6A => {
                let value = self.fetch8(bus) as i8 as i16 as u16;
                self.push(bus, value);
            }
            0x6B => {
                let m = self.decode_modrm(bus);
                let value = self.read_rm16(bus, &m);
                let imm = self.fetch8(bus) as i8 as i16;
                let result = (value as i16).wrapping_mul(imm) as u16;
                self.r[m.reg as usize] = result;
                let overflow = result as i32 != (value as i16 as i32) * (imm as i32);
                self.flags &= !(FLAG_CF | FLAG_OF);
                if overflow {
                    self.flags |= FLAG_CF | FLAG_OF;
                }
            }

            0x70..=0x7F => {
                let rel = self.fetch8(bus) as i8 as i16 as u16;
                if self.condition(opcode & 0x0F) {
                    self.ip = self.ip.wrapping_add(rel);
                }
            }

            0x80 | 0x81 | 0x82 | 0x83 => {
                let width = if opcode == 0x81 || opcode == 0x83 {
                    16
                } else {
                    8
                };
                let m = self.decode_modrm(bus);
                let op = m.reg;
                let imm = match opcode {
                    0x80 | 0x82 => self.fetch8(bus) as u16,
                    0x83 => self.fetch8(bus) as i8 as i16 as u16,
                    _ => self.fetch16(bus),
                };
                let dst = if width == 8 {
                    self.read_rm8(bus, &m) as u16
                } else {
                    self.read_rm16(bus, &m)
                };
                let result = self.alu(op, dst, imm, width);
                if op != 7 {
                    if width == 8 {
                        self.write_rm8(bus, &m, result as u8);
                    } else {
                        self.write_rm16(bus, &m, result);
                    }
                }
            }

            0x84 | 0x85 => {
                let width = if opcode == 0x84 { 8 } else { 16 };
                let m = self.decode_modrm(bus);
                let (dst, src) = if width == 8 {
                    (self.read_rm8(bus, &m) as u16, self.reg8(m.reg) as u16)
                } else {
                    (self.read_rm16(bus, &m), self.r[m.reg as usize])
                };
                self.alu(4, dst, src, width);
            }
            0x86 | 0x87 => {
                let width = if opcode == 0x86 { 8 } else { 16 };
                let m = self.decode_modrm(bus);
                if width == 8 {
                    let rm = self.read_rm8(bus, &m);
                    let reg = self.reg8(m.reg);
                    self.write_rm8(bus, &m, reg);
                    self.set_reg8(m.reg, rm);
                } else {
                    let rm = self.read_rm16(bus, &m);
                    let reg = self.r[m.reg as usize];
                    self.write_rm16(bus, &m, reg);
                    self.r[m.reg as usize] = rm;
                }
            }
            0x88..=0x8B => {
                let width = if opcode == 0x88 || opcode == 0x8A {
                    8
                } else {
                    16
                };
                let m = self.decode_modrm(bus);
                let to_reg = opcode == 0x8A || opcode == 0x8B;
                if width == 8 {
                    let value = if to_reg {
                        self.read_rm8(bus, &m)
                    } else {
                        self.reg8(m.reg)
                    };
                    if to_reg {
                        self.set_reg8(m.reg, value);
                    } else {
                        self.write_rm8(bus, &m, value);
                    }
                } else {
                    let value = if to_reg {
                        self.read_rm16(bus, &m)
                    } else {
                        self.r[m.reg as usize]
                    };
                    if to_reg {
                        self.r[m.reg as usize] = value;
                    } else {
                        self.write_rm16(bus, &m, value);
                    }
                }
            }
            0x8C => {
                let m = self.decode_modrm(bus);
                let value = self.sregs[(m.reg & 3) as usize];
                self.write_rm16(bus, &m, value);
            }
            0x8D => {
                let m = self.decode_modrm(bus);
                self.r[m.reg as usize] = m.addr;
            }
            0x8E => {
                let m = self.decode_modrm(bus);
                let value = self.read_rm16(bus, &m);
                self.sregs[(m.reg & 3) as usize] = value;
            }
            0x8F => {
                let m = self.decode_modrm(bus);
                let value = self.pop(bus);
                self.write_rm16(bus, &m, value);
            }

            0x90..=0x97 => {
                let index = (opcode & 7) as usize;
                let value = self.r[AX];
                self.r[AX] = self.r[index];
                self.r[index] = value;
            }
            0x98 => {
                self.r[AX] = (self.r[AX] as u8 as i8 as i16) as u16;
            }
            0x99 => {
                self.r[DX] = if self.r[AX] & 0x8000 != 0 { 0xFFFF } else { 0 };
            }
            0x9A => {
                let offset = self.fetch16(bus);
                let segment = self.fetch16(bus);
                let cs = self.sregs[CS];
                self.push(bus, cs);
                let ip = self.ip;
                self.push(bus, ip);
                self.sregs[CS] = segment;
                self.ip = offset;
            }
            0x9B => {}
            0x9C => {
                let flags = self.flags | 0xF002;
                self.push(bus, flags);
            }
            0x9D => {
                let flags = self.pop(bus);
                self.flags = (flags & 0x0FFF) | 0xF002;
            }
            0x9E => {
                let ah = (self.r[AX] >> 8) as u8;
                self.flags = (self.flags & 0xFF00) | (ah as u16 & 0xD5) | 0x02;
            }
            0x9F => {
                let low = (self.flags & 0xFF) as u8 | 0x02;
                self.r[AX] = (self.r[AX] & 0x00FF) | ((low as u16) << 8);
            }

            0xA0 | 0xA1 => {
                let offset = self.fetch16(bus);
                let segment = self.seg_override.unwrap_or(DS as u16) as usize;
                let addr = Self::physical(self.sregs[segment as usize], offset);
                if opcode == 0xA0 {
                    self.set_reg8(0, bus.read8(addr));
                } else {
                    self.r[AX] = bus.read16(addr);
                }
            }
            0xA2 | 0xA3 => {
                let offset = self.fetch16(bus);
                let segment = self.seg_override.unwrap_or(DS as u16) as usize;
                let addr = Self::physical(self.sregs[segment as usize], offset);
                if opcode == 0xA2 {
                    bus.write8(addr, self.r[AX] as u8);
                } else {
                    bus.write16(addr, self.r[AX]);
                }
            }
            0xA8 | 0xA9 => {
                let width = if opcode == 0xA8 { 8 } else { 16 };
                let imm = if width == 8 {
                    self.fetch8(bus) as u16
                } else {
                    self.fetch16(bus)
                };
                let acc = if width == 8 {
                    self.r[AX] & 0xFF
                } else {
                    self.r[AX]
                };
                self.alu(4, acc, imm, width);
            }
            0xA4..=0xA7 | 0xAA..=0xAF => return self.string_op(bus, opcode),

            0xB0..=0xB7 => {
                let value = self.fetch8(bus);
                self.set_reg8(opcode & 7, value);
            }
            0xB8..=0xBF => {
                let value = self.fetch16(bus);
                self.r[(opcode & 7) as usize] = value;
            }

            0xC0 | 0xC1 | 0xD0 | 0xD1 | 0xD2 | 0xD3 => {
                let width = if opcode & 1 == 0 { 8 } else { 16 };
                let m = self.decode_modrm(bus);
                let count = match opcode {
                    0xC0 | 0xC1 => self.fetch8(bus),
                    0xD0 | 0xD1 => 1,
                    _ => self.r[CX] as u8,
                };
                let value = if width == 8 {
                    self.read_rm8(bus, &m) as u16
                } else {
                    self.read_rm16(bus, &m)
                };
                let result = self.shift(m.reg, value, count, width);
                if width == 8 {
                    self.write_rm8(bus, &m, result as u8);
                } else {
                    self.write_rm16(bus, &m, result);
                }
            }
            0xC2 => {
                let ip = self.pop(bus);
                let extra = self.fetch16(bus);
                self.ip = ip;
                self.r[SP] = self.r[SP].wrapping_add(extra);
            }
            0xC3 => {
                self.ip = self.pop(bus);
            }
            0xC4 | 0xC5 => {
                let m = self.decode_modrm(bus);
                let offset = bus.read16(Self::physical(m.seg, m.addr));
                let segment = bus.read16(Self::physical(m.seg, m.addr.wrapping_add(2)));
                if opcode == 0xC4 {
                    self.sregs[ES] = segment;
                } else {
                    self.sregs[DS] = segment;
                }
                self.r[m.reg as usize] = offset;
            }
            0xC6 | 0xC7 => {
                let width = if opcode == 0xC6 { 8 } else { 16 };
                let m = self.decode_modrm(bus);
                if width == 8 {
                    let value = self.fetch8(bus);
                    self.write_rm8(bus, &m, value);
                } else {
                    let value = self.fetch16(bus);
                    self.write_rm16(bus, &m, value);
                }
            }
            0xC8 => {
                let size = self.fetch16(bus);
                let _nesting = self.fetch8(bus);
                let bp = self.r[BP];
                self.push(bus, bp);
                self.r[BP] = self.r[SP];
                self.r[SP] = self.r[SP].wrapping_sub(size);
            }
            0xC9 => {
                self.r[SP] = self.r[BP];
                self.r[BP] = self.pop(bus);
            }
            0xCA => {
                let ip = self.pop(bus);
                let cs = self.pop(bus);
                let extra = self.fetch16(bus);
                self.ip = ip;
                self.sregs[CS] = cs;
                self.r[SP] = self.r[SP].wrapping_add(extra);
            }
            0xCB => {
                self.ip = self.pop(bus);
                self.sregs[CS] = self.pop(bus);
            }
            0xCC => return self.software_interrupt(bus, 3),
            0xCD => {
                let vector = self.fetch8(bus);
                return self.software_interrupt(bus, vector);
            }
            0xCE => {
                if self.flags & FLAG_OF != 0 {
                    return self.software_interrupt(bus, 4);
                }
            }
            0xCF => {
                self.ip = self.pop(bus);
                self.sregs[CS] = self.pop(bus);
                let flags = self.pop(bus);
                self.flags = (flags & 0x0FFF) | 0xF002;
            }

            0xD4 => {
                let base = self.fetch8(bus);
                let al = self.r[AX] as u8;
                if base == 0 {
                    return self.software_interrupt(bus, 0);
                }
                self.set_reg8(4, al / base);
                self.set_reg8(0, al % base);
                self.set_szp(self.r[AX] & 0xFF, 8);
            }
            0xD5 => {
                let base = self.fetch8(bus);
                let al =
                    (self.r[AX] as u8).wrapping_add(((self.r[AX] >> 8) as u8).wrapping_mul(base));
                self.set_reg8(0, al);
                self.set_reg8(4, 0);
                self.set_szp(al as u16, 8);
            }
            0xD6 => {
                self.set_reg8(0, if self.flags & FLAG_CF != 0 { 0xFF } else { 0 });
            }
            0xD7 => {
                let addr =
                    Self::physical(self.sregs[DS], self.r[BX].wrapping_add(self.r[AX] & 0xFF));
                self.set_reg8(0, bus.read8(addr));
            }
            0xD8..=0xDF => {
                let m = self.decode_modrm(bus);
                let result = self.fpu.execute(bus, opcode, &m, &mut self.flags);
                // FNSTSW AX (DF E0).
                if opcode == 0xDF && m.is_reg && m.reg == 4 && m.rm == 0 {
                    self.r[AX] = self.fpu.status_word();
                }
                return result;
            }

            0xE0..=0xE3 => {
                let rel = self.fetch8(bus) as i8 as i16 as u16;
                if opcode == 0xE3 {
                    // JCXZ tests CX without changing it.
                    if self.r[CX] == 0 {
                        self.ip = self.ip.wrapping_add(rel);
                    }
                } else {
                    // LOOP/LOOPE/LOOPNE decrement first, then test.
                    self.r[CX] = self.r[CX].wrapping_sub(1);
                    let take = match opcode {
                        0xE0 => self.r[CX] != 0 && self.flags & FLAG_ZF == 0,
                        0xE1 => self.r[CX] != 0 && self.flags & FLAG_ZF != 0,
                        _ => self.r[CX] != 0,
                    };
                    if take {
                        self.ip = self.ip.wrapping_add(rel);
                    }
                }
            }
            0xE4 => {
                let port = self.fetch8(bus) as u16;
                self.set_reg8(0, bus.in8(port));
            }
            0xE5 => {
                let port = self.fetch8(bus) as u16;
                self.r[AX] = bus.in16(port);
            }
            0xE6 => {
                let port = self.fetch8(bus) as u16;
                bus.out8(port, self.r[AX] as u8);
            }
            0xE7 => {
                let port = self.fetch8(bus) as u16;
                bus.out16(port, self.r[AX]);
            }
            0xE8 => {
                let rel = self.fetch16(bus);
                let ip = self.ip;
                self.push(bus, ip);
                self.ip = self.ip.wrapping_add(rel);
            }
            0xE9 => {
                let rel = self.fetch16(bus);
                self.ip = self.ip.wrapping_add(rel);
            }
            0xEA => {
                let offset = self.fetch16(bus);
                let segment = self.fetch16(bus);
                self.sregs[CS] = segment;
                self.ip = offset;
            }
            0xEB => {
                let rel = self.fetch8(bus) as i8 as i16 as u16;
                self.ip = self.ip.wrapping_add(rel);
            }
            0xEC => {
                let port = self.r[DX];
                self.set_reg8(0, bus.in8(port));
            }
            0xED => {
                let port = self.r[DX];
                self.r[AX] = bus.in16(port);
            }
            0xEE => {
                let port = self.r[DX];
                bus.out8(port, self.r[AX] as u8);
            }
            0xEF => {
                let port = self.r[DX];
                bus.out16(port, self.r[AX]);
            }

            0xF1 => return Step::Fault(opcode),
            0xF4 => {
                self.halted = true;
                return Step::Halted;
            }
            0xF5 => {
                self.flags ^= FLAG_CF;
            }
            0xF6 | 0xF7 => {
                let width = if opcode == 0xF6 { 8 } else { 16 };
                let m = self.decode_modrm(bus);
                let op = m.reg;
                let value = if width == 8 {
                    self.read_rm8(bus, &m) as u16
                } else {
                    self.read_rm16(bus, &m)
                };
                match op {
                    0 | 1 => {
                        let imm = if width == 8 {
                            self.fetch8(bus) as u16
                        } else {
                            self.fetch16(bus)
                        };
                        self.alu(4, value, imm, width);
                    }
                    2 => {
                        if width == 8 {
                            self.write_rm8(bus, &m, !(value as u8));
                        } else {
                            self.write_rm16(bus, &m, !value);
                        }
                    }
                    3 => {
                        let result = self.alu(5, 0, value, width);
                        if width == 8 {
                            self.write_rm8(bus, &m, result as u8);
                        } else {
                            self.write_rm16(bus, &m, result);
                        }
                    }
                    4 => {
                        if width == 8 {
                            let result = (self.r[AX] & 0xFF) * value;
                            self.r[AX] = result;
                            let overflow = (result >> 8) != 0;
                            self.flags &= !(FLAG_CF | FLAG_OF);
                            if overflow {
                                self.flags |= FLAG_CF | FLAG_OF;
                            }
                        } else {
                            let result = (self.r[AX] as u32) * (value as u32);
                            self.r[AX] = result as u16;
                            self.r[DX] = (result >> 16) as u16;
                            let overflow = (result >> 16) != 0;
                            self.flags &= !(FLAG_CF | FLAG_OF);
                            if overflow {
                                self.flags |= FLAG_CF | FLAG_OF;
                            }
                        }
                    }
                    5 => {
                        if width == 8 {
                            let result = (self.r[AX] as u8 as i8 as i16)
                                .wrapping_mul(value as u8 as i8 as i16);
                            self.r[AX] = result as u16;
                            let overflow = result < -128 || result > 127;
                            self.flags &= !(FLAG_CF | FLAG_OF);
                            if overflow {
                                self.flags |= FLAG_CF | FLAG_OF;
                            }
                        } else {
                            let result =
                                (self.r[AX] as i16 as i32).wrapping_mul(value as i16 as i32);
                            self.r[AX] = result as u16;
                            self.r[DX] = (result >> 16) as u16;
                            let overflow = result < -32768 || result > 32767;
                            self.flags &= !(FLAG_CF | FLAG_OF);
                            if overflow {
                                self.flags |= FLAG_CF | FLAG_OF;
                            }
                        }
                    }
                    6 => {
                        if width == 8 {
                            if value == 0 {
                                return self.software_interrupt(bus, 0);
                            }
                            let dividend = self.r[AX];
                            let quotient = dividend / value;
                            let remainder = dividend % value;
                            if quotient > 0xFF {
                                return self.software_interrupt(bus, 0);
                            }
                            self.set_reg8(0, quotient as u8);
                            self.set_reg8(4, remainder as u8);
                        } else {
                            if value == 0 {
                                return self.software_interrupt(bus, 0);
                            }
                            let dividend = ((self.r[DX] as u32) << 16) | self.r[AX] as u32;
                            let quotient = dividend / value as u32;
                            let remainder = dividend % value as u32;
                            if quotient > 0xFFFF {
                                return self.software_interrupt(bus, 0);
                            }
                            self.r[AX] = quotient as u16;
                            self.r[DX] = remainder as u16;
                        }
                    }
                    7 => {
                        if width == 8 {
                            if value == 0 {
                                return self.software_interrupt(bus, 0);
                            }
                            let dividend = self.r[AX] as i16;
                            let divisor = value as u8 as i8 as i16;
                            let Some(quotient) = dividend.checked_div(divisor) else {
                                return self.software_interrupt(bus, 0);
                            };
                            let remainder = dividend % divisor;
                            if quotient < -128 || quotient > 127 {
                                return self.software_interrupt(bus, 0);
                            }
                            self.set_reg8(0, quotient as u8);
                            self.set_reg8(4, remainder as u8);
                        } else {
                            if value == 0 {
                                return self.software_interrupt(bus, 0);
                            }
                            let dividend = (((self.r[DX] as u32) << 16) | self.r[AX] as u32) as i32;
                            let divisor = value as i16 as i32;
                            let Some(quotient) = dividend.checked_div(divisor) else {
                                return self.software_interrupt(bus, 0);
                            };
                            let remainder = dividend % divisor;
                            if quotient < -32768 || quotient > 32767 {
                                return self.software_interrupt(bus, 0);
                            }
                            self.r[AX] = quotient as u16;
                            self.r[DX] = remainder as u16;
                        }
                    }
                    _ => {}
                }
            }
            0xF8 => self.flags &= !FLAG_CF,
            0xF9 => self.flags |= FLAG_CF,
            0xFA => self.flags &= !FLAG_IF,
            0xFB => self.flags |= FLAG_IF,
            0xFC => self.flags &= !FLAG_DF,
            0xFD => self.flags |= FLAG_DF,
            0xFE => {
                let m = self.decode_modrm(bus);
                let value = self.read_rm8(bus, &m);
                let result = match m.reg {
                    0 => value.wrapping_add(1),
                    1 => value.wrapping_sub(1),
                    _ => return Step::Fault(opcode),
                };
                self.write_rm8(bus, &m, result);
                self.set_szp(result as u16, 8);
            }
            0xFF => {
                let m = self.decode_modrm(bus);
                match m.reg {
                    0 | 1 => {
                        let value = self.read_rm16(bus, &m);
                        let result = if m.reg == 0 {
                            value.wrapping_add(1)
                        } else {
                            value.wrapping_sub(1)
                        };
                        self.write_rm16(bus, &m, result);
                        self.set_szp(result, 16);
                    }
                    2 => {
                        let target = self.read_rm16(bus, &m);
                        let ip = self.ip;
                        self.push(bus, ip);
                        self.ip = target;
                    }
                    3 => {
                        let offset = self.read_rm16(bus, &m);
                        let segment = bus.read16(Self::physical(m.seg, m.addr.wrapping_add(2)));
                        let cs = self.sregs[CS];
                        self.push(bus, cs);
                        let ip = self.ip;
                        self.push(bus, ip);
                        self.sregs[CS] = segment;
                        self.ip = offset;
                    }
                    4 => {
                        self.ip = self.read_rm16(bus, &m);
                    }
                    5 => {
                        let offset = self.read_rm16(bus, &m);
                        let segment = bus.read16(Self::physical(m.seg, m.addr.wrapping_add(2)));
                        self.sregs[CS] = segment;
                        self.ip = offset;
                    }
                    6 => {
                        let value = self.read_rm16(bus, &m);
                        self.push(bus, value);
                    }
                    _ => return Step::Fault(opcode),
                }
            }

            _ => return Step::Fault(opcode),
        }

        Step::Ok
    }

    fn string_op<B: Bus>(&mut self, bus: &mut B, opcode: u8) -> Step {
        let width = if opcode & 1 == 0 { 8 } else { 16 };
        let step = if width == 8 { 1u16 } else { 2 };
        let decrement = self.flags & FLAG_DF != 0;
        let rep = self.rep;
        let mut count = if rep.is_some() { self.r[CX] } else { 1 };

        let advance = |value: u16| {
            if decrement {
                value.wrapping_sub(step)
            } else {
                value.wrapping_add(step)
            }
        };

        while count > 0 {
            match opcode {
                0xA4 | 0xA5 => {
                    let src_seg = self.seg_override.unwrap_or(DS as u16) as usize;
                    let src = Self::physical(self.sregs[src_seg], self.r[SI]);
                    let dst = Self::physical(self.sregs[ES], self.r[DI]);
                    if width == 8 {
                        let value = bus.read8(src);
                        bus.write8(dst, value);
                    } else {
                        let value = bus.read16(src);
                        bus.write16(dst, value);
                    }
                    self.r[SI] = advance(self.r[SI]);
                    self.r[DI] = advance(self.r[DI]);
                }
                0xA6 | 0xA7 => {
                    let src_seg = self.seg_override.unwrap_or(DS as u16) as usize;
                    let src = Self::physical(self.sregs[src_seg], self.r[SI]);
                    let dst = Self::physical(self.sregs[ES], self.r[DI]);
                    let (a, b) = if width == 8 {
                        (bus.read8(src) as u16, bus.read8(dst) as u16)
                    } else {
                        (bus.read16(src), bus.read16(dst))
                    };
                    self.alu(7, a, b, width);
                    self.r[SI] = advance(self.r[SI]);
                    self.r[DI] = advance(self.r[DI]);
                }
                0xAA | 0xAB => {
                    let dst = Self::physical(self.sregs[ES], self.r[DI]);
                    if width == 8 {
                        bus.write8(dst, self.r[AX] as u8);
                    } else {
                        bus.write16(dst, self.r[AX]);
                    }
                    self.r[DI] = advance(self.r[DI]);
                }
                0xAC | 0xAD => {
                    let src_seg = self.seg_override.unwrap_or(DS as u16) as usize;
                    let src = Self::physical(self.sregs[src_seg], self.r[SI]);
                    if width == 8 {
                        let value = bus.read8(src);
                        self.set_reg8(0, value);
                    } else {
                        self.r[AX] = bus.read16(src);
                    }
                    self.r[SI] = advance(self.r[SI]);
                }
                0xAE | 0xAF => {
                    let dst = Self::physical(self.sregs[ES], self.r[DI]);
                    let (a, b) = if width == 8 {
                        (self.r[AX] & 0xFF, bus.read8(dst) as u16)
                    } else {
                        (self.r[AX], bus.read16(dst))
                    };
                    self.alu(7, a, b, width);
                    self.r[DI] = advance(self.r[DI]);
                }
                _ => return Step::Fault(opcode),
            }

            if rep.is_some() {
                count -= 1;
                self.r[CX] = count;
                let is_compare = matches!(opcode, 0xA6 | 0xA7 | 0xAE | 0xAF);
                if is_compare {
                    let zf = self.flags & FLAG_ZF != 0;
                    match rep {
                        Some(0xF3) if !zf => break,
                        Some(0xF2) if zf => break,
                        _ => {}
                    }
                }
            } else {
                break;
            }
        }
        Step::Ok
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ModRm {
    pub(crate) reg: u8,
    pub(crate) rm: u8,
    pub(crate) is_reg: bool,
    pub(crate) addr: u16,
    pub(crate) seg: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn alu_add_sets_flags() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        cpu.sregs[CS] = 0x1000;
        cpu.ip = 0;
        // MOV AX, 0x0FFF; ADD AX, 1
        mem.data[0x10000..0x10005].copy_from_slice(&[0xB8, 0xFF, 0x0F, 0x05, 0x01]);
        assert_eq!(cpu.step(&mut mem), Step::Ok);
        assert_eq!(cpu.r[AX], 0x0FFF);
        assert_eq!(cpu.step(&mut mem), Step::Ok);
        assert_eq!(cpu.r[AX], 0x1000);
        assert_eq!(cpu.flags & FLAG_AF, FLAG_AF);
        assert_eq!(cpu.flags & FLAG_ZF, 0);
    }

    #[test]
    fn call_and_ret() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        cpu.sregs[CS] = 0x1000;
        cpu.sregs[SS] = 0x2000;
        cpu.r[SP] = 0xFFFE;
        cpu.ip = 0;
        // MOV AX, 1; CALL +3; MOV AX, 2; RET
        mem.data[0x10000..0x1000A]
            .copy_from_slice(&[0xB8, 0x01, 0x00, 0xE8, 0x03, 0x00, 0xB8, 0x02, 0x00, 0xC3]);
        assert_eq!(cpu.step(&mut mem), Step::Ok);
        assert_eq!(cpu.r[AX], 1);
        assert_eq!(cpu.step(&mut mem), Step::Ok);
        assert_eq!(cpu.ip, 9);
        assert_eq!(cpu.step(&mut mem), Step::Ok);
        assert_eq!(cpu.ip, 6);
        assert_eq!(cpu.step(&mut mem), Step::Ok);
        assert_eq!(cpu.r[AX], 2);
    }

    #[test]
    fn string_store_with_rep() {
        let mut cpu = Cpu::new();
        let mut mem = Mem::new();
        cpu.sregs[CS] = 0x1000;
        cpu.sregs[ES] = 0x3000;
        cpu.r[DI] = 0;
        cpu.r[CX] = 4;
        cpu.r[AX] = 0x4141;
        cpu.ip = 0;
        // REP STOSW
        mem.data[0x10000..0x10002].copy_from_slice(&[0xF3, 0xAB]);
        assert_eq!(cpu.step(&mut mem), Step::Ok);
        assert_eq!(cpu.r[CX], 0);
        assert_eq!(mem.data[0x30000], 0x41);
        assert_eq!(mem.data[0x30007], 0x41);
    }
}
