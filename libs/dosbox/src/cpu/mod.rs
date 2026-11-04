//! An 80386 interpreter, with the 486 additions commonly used by DOS programs.
//!
//! Real mode, 16- and 32-bit protected mode, paging and hardware task
//! switching are implemented; virtual-8086 mode is not. The CPU identifies
//! itself consistently as a 386 through the FLAGS and `PUSH SP` tests, and
//! raises `INT 6` for undefined opcodes.
//!
//! Interrupts in real mode go through the guest's interrupt vector table, like
//! on hardware. The machine seeds that table with stubs in ROM of the form
//! `<callback n>; IRET`, where the callback instruction ([`CALLBACK_OPCODE`])
//! is what enters the Rust services through [`Bus::interrupt`]. A guest can hook
//! a vector, chain to the previous one and return with `IRET`, and the service
//! never has to know.
//!
//! Instruction costs are counted in [`Cpu::cycles`]: one per instruction, plus
//! one per `REP` iteration and whatever a service reports through
//! [`Bus::take_cost`]. `REP` runs in bounded chunks and resumes, so no single
//! [`Cpu::step`] does unbounded work.
//!
//! Memory, ports and interrupts are supplied by the [`Bus`] trait, so the same
//! CPU runs against the FEOS-DOS machine, a host-test memory or a debugger.

mod ops;
mod ops0f;
mod pmode;
#[cfg(test)]
mod tests;

use alloc::vec;
use alloc::vec::Vec;

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
pub const FS: usize = 4;
pub const GS: usize = 5;

/// Carry flag.
pub const FLAG_CF: u32 = 0x0001;
/// Parity flag.
pub const FLAG_PF: u32 = 0x0004;
/// Auxiliary carry flag.
pub const FLAG_AF: u32 = 0x0010;
/// Zero flag.
pub const FLAG_ZF: u32 = 0x0040;
/// Sign flag.
pub const FLAG_SF: u32 = 0x0080;
/// Trap flag.
pub const FLAG_TF: u32 = 0x0100;
/// Interrupt-enable flag.
pub const FLAG_IF: u32 = 0x0200;
/// Direction flag.
pub const FLAG_DF: u32 = 0x0400;
/// Overflow flag.
pub const FLAG_OF: u32 = 0x0800;
/// I/O privilege level.
pub const FLAG_IOPL: u32 = 0x3000;
/// Nested task flag.
pub const FLAG_NT: u32 = 0x4000;
/// Resume flag.
pub const FLAG_RF: u32 = 0x1_0000;
/// Virtual-8086 flag. It can't be set here: virtual-8086 mode isn't implemented.
pub const FLAG_VM: u32 = 0x2_0000;

const ARITH_FLAGS: u32 = FLAG_CF | FLAG_PF | FLAG_AF | FLAG_ZF | FLAG_SF | FLAG_OF;

/// The first byte of the callback instruction: `FE 38 n`, group 4's undefined
/// `/7` form. It's only honoured in real mode from the ROM area at
/// [`CALLBACK_ROM_BASE`] and up, so a guest can't reach it from its own code.
/// `FE 39 n` is the same for far calls and handlers, where no `IRET` frame is on
/// top of the stack, so the service's flags aren't patched into one.
pub const CALLBACK_OPCODE: u8 = 0xFE;
/// The modrm byte of the callback instruction.
pub const CALLBACK_MODRM: u8 = 0x38;
/// Lowest physical address a callback instruction is honoured at.
pub const CALLBACK_ROM_BASE: u32 = 0xF_0000;

/// How many iterations one [`Cpu::step`] of a `REP` instruction runs before it
/// resumes on the next step, which is also when pending interrupts are taken.
const REP_CHUNK: u32 = 1024;
/// The same limit for the fast paths over plain RAM.
const REP_CHUNK_FAST: u32 = 8192;
/// An instruction with more prefixes than this raises `INT 6`.
const MAX_INSTRUCTION_LEN: usize = 15;

const TLB_SIZE: usize = 512;

/// What [`Cpu::step`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// One instruction retired, or one chunk of a `REP` instruction.
    Ok,
    /// The instruction needs input that is not available yet. The instruction
    /// pointer is left on the callback instruction, interrupts are enabled, and
    /// the stacked frame is untouched, so it retries on the next step.
    Blocked,
    /// `HLT` was executed, or a service terminated the machine's program.
    Halted,
    /// The CPU shut down after a triple fault; the byte is the last vector.
    Fault(u8),
}

/// What an interrupt handler wants the CPU to do next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InterruptAction {
    /// The service completed; the stub's `IRET` runs next.
    Continue,
    /// The service needs input. Retry the callback later.
    Blocked,
    /// The service ended the machine's foreground program.
    Halt,
}

/// The register file as seen by a service.
///
/// Services may read and write the general registers, segment registers and
/// flags. Segment writes take effect as real-mode segments when the service
/// returns. The instruction pointer stays with the CPU, because the stub's
/// `IRET` is what leaves the service.
pub struct CpuRegs<'a> {
    pub r: &'a mut [u32; 8],
    pub sregs: &'a mut [u16; 6],
    pub flags: &'a mut u32,
}

impl CpuRegs<'_> {
    pub fn ax(&self) -> u16 {
        self.r[AX] as u16
    }
    pub fn bx(&self) -> u16 {
        self.r[BX] as u16
    }
    pub fn cx(&self) -> u16 {
        self.r[CX] as u16
    }
    pub fn dx(&self) -> u16 {
        self.r[DX] as u16
    }
    pub fn si(&self) -> u16 {
        self.r[SI] as u16
    }
    pub fn di(&self) -> u16 {
        self.r[DI] as u16
    }
    pub fn bp(&self) -> u16 {
        self.r[BP] as u16
    }
    pub fn sp(&self) -> u16 {
        self.r[SP] as u16
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

    fn set_low16(&mut self, index: usize, value: u16) {
        self.r[index] = (self.r[index] & 0xFFFF_0000) | value as u32;
    }
    fn set_low8(&mut self, index: usize, value: u8) {
        self.r[index] = (self.r[index] & 0xFFFF_FF00) | value as u32;
    }
    fn set_high8(&mut self, index: usize, value: u8) {
        self.r[index] = (self.r[index] & 0xFFFF_00FF) | ((value as u32) << 8);
    }

    pub fn set_ax(&mut self, value: u16) {
        self.set_low16(AX, value);
    }
    pub fn set_bx(&mut self, value: u16) {
        self.set_low16(BX, value);
    }
    pub fn set_bh(&mut self, value: u8) {
        self.set_high8(BX, value);
    }
    pub fn set_bl(&mut self, value: u8) {
        self.set_low8(BX, value);
    }
    pub fn set_cx(&mut self, value: u16) {
        self.set_low16(CX, value);
    }
    pub fn set_ch(&mut self, value: u8) {
        self.set_high8(CX, value);
    }
    pub fn set_cl(&mut self, value: u8) {
        self.set_low8(CX, value);
    }
    pub fn set_dx(&mut self, value: u16) {
        self.set_low16(DX, value);
    }
    pub fn set_al(&mut self, value: u8) {
        self.set_low8(AX, value);
    }
    pub fn set_ah(&mut self, value: u8) {
        self.set_high8(AX, value);
    }
    pub fn set_dl(&mut self, value: u8) {
        self.set_low8(DX, value);
    }
    pub fn set_dh(&mut self, value: u8) {
        self.set_high8(DX, value);
    }
    pub fn set_si(&mut self, value: u16) {
        self.set_low16(SI, value);
    }
    pub fn set_di(&mut self, value: u16) {
        self.set_low16(DI, value);
    }
    pub fn set_bp(&mut self, value: u16) {
        self.set_low16(BP, value);
    }
    pub fn set_sp(&mut self, value: u16) {
        self.set_low16(SP, value);
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
    pub fn set_ss(&mut self, value: u16) {
        self.sregs[SS] = value;
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

/// Devices the CPU can reach. Addresses are physical.
pub trait Bus: 'static {
    fn read8(&mut self, addr: u32) -> u8;
    fn write8(&mut self, addr: u32, value: u8);

    fn read16(&mut self, addr: u32) -> u16 {
        self.read8(addr) as u16 | ((self.read8(addr.wrapping_add(1)) as u16) << 8)
    }

    fn write16(&mut self, addr: u32, value: u16) {
        self.write8(addr, value as u8);
        self.write8(addr.wrapping_add(1), (value >> 8) as u8);
    }

    fn read32(&mut self, addr: u32) -> u32 {
        self.read16(addr) as u32 | ((self.read16(addr.wrapping_add(2)) as u32) << 16)
    }

    fn write32(&mut self, addr: u32, value: u32) {
        self.write16(addr, value as u16);
        self.write16(addr.wrapping_add(2), (value >> 16) as u16);
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

    fn in32(&mut self, port: u16) -> u32 {
        self.in16(port) as u32 | ((self.in16(port.wrapping_add(2)) as u32) << 16)
    }

    fn out32(&mut self, port: u16, value: u32) {
        self.out16(port, value as u16);
        self.out16(port.wrapping_add(2), (value >> 16) as u16);
    }

    /// Runs the service behind a callback instruction.
    fn interrupt(&mut self, _vector: u8, _regs: &mut CpuRegs<'_>) -> InterruptAction {
        InterruptAction::Continue
    }

    /// Tells the bus the CPU's time before a port access or service.
    fn sync(&mut self, _cycles: u64) {}

    /// The guest work done by the last service, in instruction units.
    fn take_cost(&mut self) -> u32 {
        0
    }

    /// Copies `len` bytes forward if both ranges are plain RAM and the copy is
    /// the same as a byte-by-byte one. Returning false uses the slow path.
    fn fast_copy(&mut self, _dst: u32, _src: u32, _len: u32) -> bool {
        false
    }

    /// Fills `len` bytes with `pattern`, repeated little-endian every `width`
    /// bytes, if the range is plain RAM. Returning false uses the slow path.
    fn fast_fill(&mut self, _dst: u32, _pattern: u32, _width: u32, _len: u32) -> bool {
        false
    }
}

/// Why an instruction stopped early.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Trap {
    /// A processor exception without an error code.
    Fault(u8),
    /// A processor exception with an error code.
    FaultCode(u8, u32),
    /// A service wants to be retried.
    Blocked,
    /// A service ended the foreground program.
    Terminated,
    /// `HLT` ran.
    Hlt,
}

pub(crate) type Exec<T = ()> = Result<T, Trap>;

/// A segment register's hidden part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub base: u32,
    /// Lowest valid offset: above the limit for an expand-down segment.
    pub lo: u32,
    /// Highest valid offset.
    pub hi: u32,
    /// The D/B bit.
    pub big: bool,
    pub readable: bool,
    pub writable: bool,
    pub code: bool,
    pub conforming: bool,
    pub dpl: u8,
}

impl Seg {
    const fn real(selector: u16) -> Self {
        Self {
            base: (selector as u32) << 4,
            lo: 0,
            hi: 0xFFFF,
            big: false,
            readable: true,
            writable: true,
            code: false,
            conforming: false,
            dpl: 0,
        }
    }
}

/// A descriptor table register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TableReg {
    pub base: u32,
    pub limit: u32,
}

/// The LDT or task register: a selector plus its cached descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SysSeg {
    pub sel: u16,
    pub base: u32,
    pub limit: u32,
    /// The descriptor type, for the task register.
    pub kind: u8,
}

/// One decoded ModR/M operand.
#[derive(Clone, Copy)]
pub(crate) struct ModRm {
    pub(crate) reg: u8,
    pub(crate) rm: u8,
    pub(crate) is_reg: bool,
    /// Effective offset, already wrapped to the address size.
    pub(crate) off: u32,
    /// Segment register index.
    pub(crate) seg: u8,
}

/// The processor state.
#[derive(Clone)]
pub struct Cpu {
    /// `EAX, ECX, EDX, EBX, ESP, EBP, ESI, EDI`.
    pub r: [u32; 8],
    /// `ES, CS, SS, DS, FS, GS` selectors.
    pub sregs: [u16; 6],
    /// The hidden parts of `sregs`.
    pub seg: [Seg; 6],
    /// `EIP`.
    pub ip: u32,
    /// Non-arithmetic flags, and whichever arithmetic flags aren't lazy.
    eflags: u32,
    /// The arithmetic flags still to be computed from the fields below.
    lazy: u32,
    lazy_res: u32,
    /// The carry-out vector of the last operation.
    lazy_cout: u32,
    lazy_bits: u32,
    pub halted: bool,
    /// Instruction units retired since reset.
    pub cycles: u64,

    pub cr0: u32,
    pub cr2: u32,
    pub cr3: u32,
    pub cr4: u32,
    pub dr: [u32; 8],
    pub gdtr: TableReg,
    pub idtr: TableReg,
    pub ldtr: SysSeg,
    pub tr: SysSeg,
    /// Current privilege level.
    pub cpl: u8,
    pmode: bool,
    paging: bool,
    tlb: Vec<u64>,

    /// STI, `MOV SS` and `POP SS` hold off interrupts for one instruction.
    inhibit: bool,
    /// The running instruction consumed its own single-step trap.
    no_trap: bool,
    start_ip: u32,
    start_sp: u32,

    // Prefix state for the instruction being decoded.
    op32: bool,
    addr32: bool,
    seg_override: Option<u8>,
    rep: u8,

    /// x87 floating-point state.
    pub fpu: crate::fpu::Fpu,

    /// Ring of recently executed instructions, for debugging.
    #[cfg(feature = "trace")]
    pub trace: [TraceEntry; TRACE_LEN],
    #[cfg(feature = "trace")]
    pub trace_index: usize,
}

/// One entry in the CPU's instruction trace.
#[cfg(feature = "trace")]
#[derive(Clone, Copy, Default, Debug)]
pub struct TraceEntry {
    pub cs: u16,
    pub ip: u32,
    pub opcode: u8,
    pub r: [u32; 8],
    pub ds: u16,
    pub es: u16,
    pub flags: u32,
}

/// Number of instructions kept in the trace ring.
#[cfg(feature = "trace")]
pub const TRACE_LEN: usize = 64;

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

struct Saved {
    eflags: u32,
    lazy: u32,
    lazy_res: u32,
    lazy_cout: u32,
    lazy_bits: u32,
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            r: [0; 8],
            sregs: [0; 6],
            seg: [Seg::real(0); 6],
            ip: 0,
            eflags: 0x0202,
            lazy: 0,
            lazy_res: 0,
            lazy_cout: 0,
            lazy_bits: 16,
            halted: false,
            cycles: 0,
            cr0: 0x10,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            dr: [0; 8],
            gdtr: TableReg {
                base: 0,
                limit: 0xFFFF,
            },
            idtr: TableReg {
                base: 0,
                limit: 0x3FF,
            },
            ldtr: SysSeg::default(),
            tr: SysSeg::default(),
            cpl: 0,
            pmode: false,
            paging: false,
            tlb: vec![0; TLB_SIZE],
            inhibit: false,
            no_trap: false,
            start_ip: 0,
            start_sp: 0,
            op32: false,
            addr32: false,
            seg_override: None,
            rep: 0,
            fpu: crate::fpu::Fpu::new(),
            #[cfg(feature = "trace")]
            trace: [TraceEntry::default(); TRACE_LEN],
            #[cfg(feature = "trace")]
            trace_index: 0,
        }
    }

    /// Resets the register file to the state the BIOS leaves at boot.
    pub fn reset(&mut self) {
        let cycles = self.cycles;
        *self = Self::new();
        self.cycles = cycles;
    }

    /// A real-mode address.
    pub const fn physical(segment: u16, offset: u16) -> u32 {
        ((segment as u32) << 4).wrapping_add(offset as u32)
    }

    /// The offset of the instruction being executed.
    pub(crate) fn instr_start(&self) -> u32 {
        self.start_ip
    }

    /// Whether the CPU is in protected mode.
    pub fn protected_mode(&self) -> bool {
        self.pmode
    }

    /// Whether a hardware interrupt can be taken before the next instruction.
    #[inline]
    pub fn interrupts_enabled(&self) -> bool {
        self.eflags & FLAG_IF != 0 && !self.inhibit
    }

    /// Sets a real-mode segment register and its hidden part.
    pub fn set_real_segment(&mut self, index: usize, selector: u16) {
        self.sregs[index] = selector;
        let hi = self.seg[index].hi;
        let big = self.seg[index].big;
        self.seg[index] = Seg::real(selector);
        self.seg[index].hi = hi;
        self.seg[index].big = big;
    }

    // ------------------------------------------------------------------
    // Registers
    // ------------------------------------------------------------------

    #[inline]
    pub fn reg8(&self, index: u8) -> u8 {
        let index = (index & 7) as usize;
        if index < 4 {
            self.r[index] as u8
        } else {
            (self.r[index - 4] >> 8) as u8
        }
    }

    #[inline]
    pub fn set_reg8(&mut self, index: u8, value: u8) {
        let index = (index & 7) as usize;
        if index < 4 {
            self.r[index] = (self.r[index] & 0xFFFF_FF00) | value as u32;
        } else {
            self.r[index - 4] = (self.r[index - 4] & 0xFFFF_00FF) | ((value as u32) << 8);
        }
    }

    #[inline]
    pub(crate) fn reg16(&self, index: u8) -> u16 {
        self.r[(index & 7) as usize] as u16
    }

    #[inline]
    pub(crate) fn set_reg16(&mut self, index: u8, value: u16) {
        let index = (index & 7) as usize;
        self.r[index] = (self.r[index] & 0xFFFF_0000) | value as u32;
    }

    /// Reads a register of width 16 or 32.
    #[inline(always)]
    pub(crate) fn regv(&self, index: u8, w: u32) -> u32 {
        if w == 32 {
            self.r[(index & 7) as usize]
        } else {
            self.reg16(index) as u32
        }
    }

    /// Writes a register of width 16 or 32.
    #[inline(always)]
    pub(crate) fn set_regv(&mut self, index: u8, w: u32, value: u32) {
        if w == 32 {
            self.r[(index & 7) as usize] = value;
        } else {
            self.set_reg16(index, value as u16);
        }
    }

    /// The operand width selected for this instruction.
    #[inline(always)]
    pub(crate) fn vw(&self) -> u32 {
        if self.op32 {
            32
        } else {
            16
        }
    }

    // ------------------------------------------------------------------
    // Flags
    // ------------------------------------------------------------------

    /// The architectural `EFLAGS` value.
    #[inline]
    pub fn flags(&self) -> u32 {
        if self.lazy == 0 {
            return self.eflags;
        }
        (self.eflags & !self.lazy) | (self.compute_lazy() & self.lazy)
    }

    /// Sets `EFLAGS` without any privilege checks.
    pub fn set_flags(&mut self, value: u32) {
        self.lazy = 0;
        self.eflags = (value | 2) & !0x28 & !FLAG_VM;
    }

    fn compute_lazy(&self) -> u32 {
        let mut out = 0;
        let top = self.lazy_bits - 1;
        let carry = self.lazy_cout;
        if (carry >> top) & 1 != 0 {
            out |= FLAG_CF;
        }
        if ((carry >> top) ^ (carry >> (top - 1))) & 1 != 0 {
            out |= FLAG_OF;
        }
        if (carry >> 3) & 1 != 0 {
            out |= FLAG_AF;
        }
        if self.lazy_res == 0 {
            out |= FLAG_ZF;
        }
        if (self.lazy_res >> top) & 1 != 0 {
            out |= FLAG_SF;
        }
        if (self.lazy_res as u8).count_ones() & 1 == 0 {
            out |= FLAG_PF;
        }
        out
    }

    #[inline]
    pub(crate) fn materialize(&mut self) {
        if self.lazy != 0 {
            self.eflags = self.flags();
            self.lazy = 0;
        }
    }

    #[inline(always)]
    fn set_lazy(&mut self, res: u32, cout: u32, bits: u32) {
        self.lazy_res = res;
        self.lazy_cout = cout;
        self.lazy_bits = bits;
        self.lazy = ARITH_FLAGS;
    }

    #[inline(always)]
    pub(crate) fn cf(&self) -> bool {
        if self.lazy & FLAG_CF != 0 {
            (self.lazy_cout >> (self.lazy_bits - 1)) & 1 != 0
        } else {
            self.eflags & FLAG_CF != 0
        }
    }

    #[inline(always)]
    pub(crate) fn zf(&self) -> bool {
        if self.lazy & FLAG_ZF != 0 {
            self.lazy_res == 0
        } else {
            self.eflags & FLAG_ZF != 0
        }
    }

    #[inline(always)]
    pub(crate) fn sf(&self) -> bool {
        if self.lazy & FLAG_SF != 0 {
            (self.lazy_res >> (self.lazy_bits - 1)) & 1 != 0
        } else {
            self.eflags & FLAG_SF != 0
        }
    }

    #[inline(always)]
    pub(crate) fn of(&self) -> bool {
        if self.lazy & FLAG_OF != 0 {
            let top = self.lazy_bits - 1;
            ((self.lazy_cout >> top) ^ (self.lazy_cout >> (top - 1))) & 1 != 0
        } else {
            self.eflags & FLAG_OF != 0
        }
    }

    #[inline(always)]
    pub(crate) fn pf(&self) -> bool {
        if self.lazy & FLAG_PF != 0 {
            (self.lazy_res as u8).count_ones() & 1 == 0
        } else {
            self.eflags & FLAG_PF != 0
        }
    }

    #[inline(always)]
    pub(crate) fn af(&self) -> bool {
        self.flags() & FLAG_AF != 0
    }

    #[inline]
    pub(crate) fn set_flag(&mut self, flag: u32, on: bool) {
        self.lazy &= !flag;
        if on {
            self.eflags |= flag;
        } else {
            self.eflags &= !flag;
        }
    }

    #[inline(always)]
    pub(crate) fn df(&self) -> bool {
        self.eflags & FLAG_DF != 0
    }

    #[inline(always)]
    pub(crate) fn iopl(&self) -> u8 {
        ((self.eflags >> 12) & 3) as u8
    }

    /// Sets ZF, SF and PF from a result, leaving the other flags alone.
    pub(crate) fn set_szp(&mut self, value: u32, bits: u32) {
        self.materialize();
        let value = value & mask(bits);
        self.eflags &= !(FLAG_ZF | FLAG_SF | FLAG_PF);
        if value == 0 {
            self.eflags |= FLAG_ZF;
        }
        if (value >> (bits - 1)) & 1 != 0 {
            self.eflags |= FLAG_SF;
        }
        if (value as u8).count_ones() & 1 == 0 {
            self.eflags |= FLAG_PF;
        }
    }

    #[inline(always)]
    pub(crate) fn alu(&mut self, op: u8, a: u32, b: u32, bits: u32) -> u32 {
        let m = mask(bits);
        let (a, b) = (a & m, b & m);
        match op & 7 {
            0 => {
                let res = a.wrapping_add(b);
                self.set_lazy(res & m, (a & b) | ((a | b) & !res), bits);
                res & m
            }
            1 => {
                let res = a | b;
                self.set_lazy(res, 0, bits);
                res
            }
            2 => {
                let res = a.wrapping_add(b).wrapping_add(u32::from(self.cf()));
                self.set_lazy(res & m, (a & b) | ((a | b) & !res), bits);
                res & m
            }
            3 => {
                let res = a.wrapping_sub(b).wrapping_sub(u32::from(self.cf()));
                self.set_lazy(res & m, (!a & b) | ((!a | b) & res), bits);
                res & m
            }
            4 => {
                let res = a & b;
                self.set_lazy(res, 0, bits);
                res
            }
            6 => {
                let res = a ^ b;
                self.set_lazy(res, 0, bits);
                res
            }
            _ => {
                let res = a.wrapping_sub(b);
                self.set_lazy(res & m, (!a & b) | ((!a | b) & res), bits);
                res & m
            }
        }
    }

    /// `INC` and `DEC`: like `ADD` and `SUB` by one, but CF is preserved.
    #[inline(always)]
    pub(crate) fn inc_dec(&mut self, value: u32, dec: bool, bits: u32) -> u32 {
        let cf = self.cf();
        let res = self.alu(if dec { 5 } else { 0 }, value, 1, bits);
        if cf {
            self.eflags |= FLAG_CF;
        } else {
            self.eflags &= !FLAG_CF;
        }
        self.lazy &= !FLAG_CF;
        res
    }

    #[inline]
    pub(crate) fn condition(&self, code: u8) -> bool {
        let result = match code >> 1 & 7 {
            0 => self.of(),
            1 => self.cf(),
            2 => self.zf(),
            3 => self.cf() || self.zf(),
            4 => self.sf(),
            5 => self.pf(),
            6 => self.sf() != self.of(),
            _ => self.zf() || self.sf() != self.of(),
        };
        result != (code & 1 != 0)
    }

    // ------------------------------------------------------------------
    // Linear and segmented memory
    // ------------------------------------------------------------------

    pub(crate) fn seg_fault(&self, seg: usize) -> Trap {
        if seg == SS {
            Trap::FaultCode(12, 0)
        } else {
            Trap::FaultCode(13, 0)
        }
    }

    /// Translates a linear address. `user` asks for a user-mode access.
    pub(crate) fn translate_as<B: Bus>(
        &mut self,
        bus: &mut B,
        linear: u32,
        write: bool,
        user: bool,
    ) -> Exec<u32> {
        if !self.paging {
            return Ok(linear);
        }
        let vpn = linear >> 12;
        let index = vpn as usize & (TLB_SIZE - 1);
        let entry = self.tlb[index];
        if (entry >> 32) as u32 == vpn + 1 {
            let bits = entry as u32;
            let ok = (!user || bits & 2 != 0)
                && (!write
                    || (bits & 4 != 0 && (bits & 1 != 0 || (!user && self.cr0 & 0x1_0000 == 0))));
            if ok {
                return Ok((bits & 0xFFFF_F000) | (linear & 0xFFF));
            }
        }
        self.walk(bus, linear, write, user)
    }

    #[cold]
    fn walk<B: Bus>(&mut self, bus: &mut B, linear: u32, write: bool, user: bool) -> Exec<u32> {
        let fault = |cpu: &mut Self, present: bool| {
            cpu.cr2 = linear;
            let code = u32::from(present) | (u32::from(write) << 1) | (u32::from(user) << 2);
            Err(Trap::FaultCode(14, code))
        };
        let pde_addr = (self.cr3 & 0xFFFF_F000) | ((linear >> 22) << 2);
        let pde = bus.read32(pde_addr);
        if pde & 1 == 0 {
            return fault(self, false);
        }
        let pte_addr = (pde & 0xFFFF_F000) | ((linear >> 10) & 0xFFC);
        let pte = bus.read32(pte_addr);
        if pte & 1 == 0 {
            return fault(self, false);
        }
        let can_write = pde & pte & 2 != 0;
        let can_user = pde & pte & 4 != 0;
        if user && !can_user {
            return fault(self, true);
        }
        if write && !can_write && (user || self.cr0 & 0x1_0000 != 0) {
            return fault(self, true);
        }
        if pde & 0x20 == 0 {
            bus.write32(pde_addr, pde | 0x20);
        }
        let new_pte = pte | 0x20 | if write { 0x40 } else { 0 };
        if new_pte != pte {
            bus.write32(pte_addr, new_pte);
        }
        let frame = pte & 0xFFFF_F000;
        let vpn = linear >> 12;
        let bits = frame
            | u32::from(can_write)
            | (u32::from(can_user) << 1)
            | (u32::from(new_pte & 0x40 != 0) << 2);
        self.tlb[vpn as usize & (TLB_SIZE - 1)] = ((vpn as u64 + 1) << 32) | bits as u64;
        Ok(frame | (linear & 0xFFF))
    }

    pub(crate) fn flush_tlb(&mut self) {
        for entry in self.tlb.iter_mut() {
            *entry = 0;
        }
    }

    #[inline(always)]
    fn translate<B: Bus>(&mut self, bus: &mut B, linear: u32, write: bool) -> Exec<u32> {
        if !self.paging {
            return Ok(linear);
        }
        let user = self.cpl == 3;
        self.translate_as(bus, linear, write, user)
    }

    /// Whether an offset's access misses the segment only because a real-mode
    /// 16-bit address wraps, where it should continue at offset zero.
    fn real_wrap(&self, seg: usize, off: u32) -> bool {
        !self.pmode && off <= 0xFFFF && self.seg[seg].hi == 0xFFFF
    }

    #[inline(always)]
    pub(crate) fn rd8<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32) -> Exec<u8> {
        let s = &self.seg[seg];
        if off < s.lo || off > s.hi || !s.readable {
            return Err(self.seg_fault(seg));
        }
        let linear = s.base.wrapping_add(off);
        let phys = self.translate(bus, linear, false)?;
        Ok(bus.read8(phys))
    }

    #[inline(always)]
    pub(crate) fn wr8<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32, value: u8) -> Exec {
        let s = &self.seg[seg];
        if off < s.lo || off > s.hi || !s.writable {
            return Err(self.seg_fault(seg));
        }
        let linear = s.base.wrapping_add(off);
        let phys = self.translate(bus, linear, true)?;
        bus.write8(phys, value);
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn rd16<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32) -> Exec<u16> {
        let s = &self.seg[seg];
        if off >= s.lo && off < s.hi && s.readable {
            let linear = s.base.wrapping_add(off);
            if !self.paging {
                return Ok(bus.read16(linear));
            }
            if linear & 0xFFF != 0xFFF {
                let phys = self.translate(bus, linear, false)?;
                return Ok(bus.read16(phys));
            }
        }
        self.rd_slow(bus, seg, off, 2).map(|value| value as u16)
    }

    #[inline(always)]
    pub(crate) fn rd32<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32) -> Exec<u32> {
        let s = &self.seg[seg];
        if off >= s.lo && s.hi >= 3 && off <= s.hi - 3 && s.readable {
            let linear = s.base.wrapping_add(off);
            if !self.paging {
                return Ok(bus.read32(linear));
            }
            if linear & 0xFFF <= 0xFFC {
                let phys = self.translate(bus, linear, false)?;
                return Ok(bus.read32(phys));
            }
        }
        self.rd_slow(bus, seg, off, 4)
    }

    #[cold]
    fn rd_slow<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32, size: u32) -> Exec<u32> {
        let wrap = self.real_wrap(seg, off);
        let mut value = 0u32;
        for index in 0..size {
            let at = if wrap {
                (off + index) & 0xFFFF
            } else {
                off.wrapping_add(index)
            };
            value |= (self.rd8(bus, seg, at)? as u32) << (index * 8);
        }
        Ok(value)
    }

    #[inline(always)]
    pub(crate) fn wr16<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32, value: u16) -> Exec {
        let s = &self.seg[seg];
        if off >= s.lo && off < s.hi && s.writable {
            let linear = s.base.wrapping_add(off);
            if !self.paging {
                bus.write16(linear, value);
                return Ok(());
            }
            if linear & 0xFFF != 0xFFF {
                let phys = self.translate(bus, linear, true)?;
                bus.write16(phys, value);
                return Ok(());
            }
        }
        self.wr_slow(bus, seg, off, 2, value as u32)
    }

    #[inline(always)]
    pub(crate) fn wr32<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32, value: u32) -> Exec {
        let s = &self.seg[seg];
        if off >= s.lo && s.hi >= 3 && off <= s.hi - 3 && s.writable {
            let linear = s.base.wrapping_add(off);
            if !self.paging {
                bus.write32(linear, value);
                return Ok(());
            }
            if linear & 0xFFF <= 0xFFC {
                let phys = self.translate(bus, linear, true)?;
                bus.write32(phys, value);
                return Ok(());
            }
        }
        self.wr_slow(bus, seg, off, 4, value)
    }

    #[cold]
    fn wr_slow<B: Bus>(
        &mut self,
        bus: &mut B,
        seg: usize,
        off: u32,
        size: u32,
        value: u32,
    ) -> Exec {
        let wrap = self.real_wrap(seg, off);
        let at = |index: u32| {
            if wrap {
                (off + index) & 0xFFFF
            } else {
                off.wrapping_add(index)
            }
        };
        // Check both ends first, so a fault doesn't leave half a write behind.
        let last = at(size - 1);
        let s = &self.seg[seg];
        if !s.writable || at(0) < s.lo || at(0) > s.hi || last < s.lo || last > s.hi {
            return Err(self.seg_fault(seg));
        }
        let base = s.base;
        self.translate(bus, base.wrapping_add(at(0)), true)?;
        self.translate(bus, base.wrapping_add(last), true)?;
        for index in 0..size {
            let byte = (value >> (index * 8)) as u8;
            self.wr8(bus, seg, at(index), byte)?;
        }
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn rdv<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32, w: u32) -> Exec<u32> {
        if w == 32 {
            self.rd32(bus, seg, off)
        } else {
            Ok(self.rd16(bus, seg, off)? as u32)
        }
    }

    #[inline(always)]
    pub(crate) fn wrv<B: Bus>(
        &mut self,
        bus: &mut B,
        seg: usize,
        off: u32,
        w: u32,
        value: u32,
    ) -> Exec {
        if w == 32 {
            self.wr32(bus, seg, off, value)
        } else {
            self.wr16(bus, seg, off, value as u16)
        }
    }

    /// Reads at the width given: 8, 16 or 32.
    #[inline(always)]
    pub(crate) fn rdw<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u32, w: u32) -> Exec<u32> {
        if w == 8 {
            Ok(self.rd8(bus, seg, off)? as u32)
        } else {
            self.rdv(bus, seg, off, w)
        }
    }

    /// Writes at the width given: 8, 16 or 32.
    #[inline(always)]
    pub(crate) fn wrw<B: Bus>(
        &mut self,
        bus: &mut B,
        seg: usize,
        off: u32,
        w: u32,
        value: u32,
    ) -> Exec {
        if w == 8 {
            self.wr8(bus, seg, off, value as u8)
        } else {
            self.wrv(bus, seg, off, w, value)
        }
    }

    /// Reads a dword through the system tables, as a supervisor access.
    pub(crate) fn sys_rd32<B: Bus>(&mut self, bus: &mut B, linear: u32) -> Exec<u32> {
        if linear & 0xFFF > 0xFFC && self.paging {
            let mut value = 0;
            for index in 0..4 {
                value |= (self.sys_rd8(bus, linear.wrapping_add(index))? as u32) << (index * 8);
            }
            return Ok(value);
        }
        let phys = self.translate_as(bus, linear, false, false)?;
        Ok(bus.read32(phys))
    }

    pub(crate) fn sys_rd8<B: Bus>(&mut self, bus: &mut B, linear: u32) -> Exec<u8> {
        let phys = self.translate_as(bus, linear, false, false)?;
        Ok(bus.read8(phys))
    }

    pub(crate) fn sys_rd16<B: Bus>(&mut self, bus: &mut B, linear: u32) -> Exec<u16> {
        Ok(self.sys_rd8(bus, linear)? as u16
            | ((self.sys_rd8(bus, linear.wrapping_add(1))? as u16) << 8))
    }

    pub(crate) fn sys_wr8<B: Bus>(&mut self, bus: &mut B, linear: u32, value: u8) -> Exec {
        let phys = self.translate_as(bus, linear, true, false)?;
        bus.write8(phys, value);
        Ok(())
    }

    pub(crate) fn sys_wr16<B: Bus>(&mut self, bus: &mut B, linear: u32, value: u16) -> Exec {
        self.sys_wr8(bus, linear, value as u8)?;
        self.sys_wr8(bus, linear.wrapping_add(1), (value >> 8) as u8)
    }

    pub(crate) fn sys_wr32<B: Bus>(&mut self, bus: &mut B, linear: u32, value: u32) -> Exec {
        self.sys_wr16(bus, linear, value as u16)?;
        self.sys_wr16(bus, linear.wrapping_add(2), (value >> 16) as u16)
    }

    // ------------------------------------------------------------------
    // Fetch and stack
    // ------------------------------------------------------------------

    #[inline(always)]
    pub(crate) fn fetch8<B: Bus>(&mut self, bus: &mut B) -> Exec<u8> {
        let s = &self.seg[CS];
        if self.ip > s.hi {
            return Err(Trap::FaultCode(13, 0));
        }
        let linear = s.base.wrapping_add(self.ip);
        let big = s.big;
        let phys = self.translate(bus, linear, false)?;
        let value = bus.read8(phys);
        self.ip = self.ip.wrapping_add(1);
        if !big {
            self.ip &= 0xFFFF;
        }
        Ok(value)
    }

    #[inline(always)]
    pub(crate) fn fetch16<B: Bus>(&mut self, bus: &mut B) -> Exec<u16> {
        let lo = self.fetch8(bus)? as u16;
        let hi = self.fetch8(bus)? as u16;
        Ok(lo | (hi << 8))
    }

    #[inline(always)]
    pub(crate) fn fetch32<B: Bus>(&mut self, bus: &mut B) -> Exec<u32> {
        let lo = self.fetch16(bus)? as u32;
        let hi = self.fetch16(bus)? as u32;
        Ok(lo | (hi << 16))
    }

    /// Fetches an immediate of width 8, 16 or 32.
    #[inline(always)]
    pub(crate) fn fetchw<B: Bus>(&mut self, bus: &mut B, w: u32) -> Exec<u32> {
        match w {
            8 => Ok(self.fetch8(bus)? as u32),
            16 => Ok(self.fetch16(bus)? as u32),
            _ => self.fetch32(bus),
        }
    }

    #[inline(always)]
    pub(crate) fn sp_big(&self) -> bool {
        self.seg[SS].big
    }

    #[inline(always)]
    pub(crate) fn sp_add(&mut self, amount: u32) {
        if self.sp_big() {
            self.r[SP] = self.r[SP].wrapping_add(amount);
        } else {
            let sp = (self.r[SP] as u16).wrapping_add(amount as u16);
            self.set_reg16(SP as u8, sp);
        }
    }

    #[inline(always)]
    pub(crate) fn push16<B: Bus>(&mut self, bus: &mut B, value: u16) -> Exec {
        let off = if self.sp_big() {
            self.r[SP].wrapping_sub(2)
        } else {
            (self.r[SP] as u16).wrapping_sub(2) as u32
        };
        self.wr16(bus, SS, off, value)?;
        self.sp_add(2u32.wrapping_neg());
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn push32<B: Bus>(&mut self, bus: &mut B, value: u32) -> Exec {
        let off = if self.sp_big() {
            self.r[SP].wrapping_sub(4)
        } else {
            (self.r[SP] as u16).wrapping_sub(4) as u32
        };
        self.wr32(bus, SS, off, value)?;
        self.sp_add(4u32.wrapping_neg());
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn pushv<B: Bus>(&mut self, bus: &mut B, value: u32, w: u32) -> Exec {
        if w == 32 {
            self.push32(bus, value)
        } else {
            self.push16(bus, value as u16)
        }
    }

    #[inline(always)]
    pub(crate) fn sp_off(&self) -> u32 {
        if self.sp_big() {
            self.r[SP]
        } else {
            self.r[SP] & 0xFFFF
        }
    }

    #[inline(always)]
    pub(crate) fn pop16<B: Bus>(&mut self, bus: &mut B) -> Exec<u16> {
        let value = self.rd16(bus, SS, self.sp_off())?;
        self.sp_add(2);
        Ok(value)
    }

    #[inline(always)]
    pub(crate) fn pop32<B: Bus>(&mut self, bus: &mut B) -> Exec<u32> {
        let value = self.rd32(bus, SS, self.sp_off())?;
        self.sp_add(4);
        Ok(value)
    }

    #[inline(always)]
    pub(crate) fn popv<B: Bus>(&mut self, bus: &mut B, w: u32) -> Exec<u32> {
        if w == 32 {
            self.pop32(bus)
        } else {
            Ok(self.pop16(bus)? as u32)
        }
    }

    // ------------------------------------------------------------------
    // ModR/M
    // ------------------------------------------------------------------

    pub(crate) fn modrm<B: Bus>(&mut self, bus: &mut B) -> Exec<ModRm> {
        let byte = self.fetch8(bus)?;
        self.modrm_byte(bus, byte)
    }

    pub(crate) fn modrm_byte<B: Bus>(&mut self, bus: &mut B, byte: u8) -> Exec<ModRm> {
        let mode = byte >> 6;
        let reg = (byte >> 3) & 7;
        let rm = byte & 7;
        if mode == 3 {
            return Ok(ModRm {
                reg,
                rm,
                is_reg: true,
                off: 0,
                seg: DS as u8,
            });
        }
        let (off, default_seg) = if self.addr32 {
            self.ea32(bus, mode, rm)?
        } else {
            self.ea16(bus, mode, rm)?
        };
        Ok(ModRm {
            reg,
            rm,
            is_reg: false,
            off,
            seg: self.seg_override.unwrap_or(default_seg as u8),
        })
    }

    fn ea16<B: Bus>(&mut self, bus: &mut B, mode: u8, rm: u8) -> Exec<(u32, usize)> {
        let (base, seg) = match rm {
            0 => (self.reg16(BX as u8).wrapping_add(self.reg16(SI as u8)), DS),
            1 => (self.reg16(BX as u8).wrapping_add(self.reg16(DI as u8)), DS),
            2 => (self.reg16(BP as u8).wrapping_add(self.reg16(SI as u8)), SS),
            3 => (self.reg16(BP as u8).wrapping_add(self.reg16(DI as u8)), SS),
            4 => (self.reg16(SI as u8), DS),
            5 => (self.reg16(DI as u8), DS),
            6 if mode == 0 => (0, DS),
            6 => (self.reg16(BP as u8), SS),
            _ => (self.reg16(BX as u8), DS),
        };
        let disp = match mode {
            0 if rm == 6 => self.fetch16(bus)?,
            1 => self.fetch8(bus)? as i8 as i16 as u16,
            2 => self.fetch16(bus)?,
            _ => 0,
        };
        Ok((base.wrapping_add(disp) as u32, seg))
    }

    fn ea32<B: Bus>(&mut self, bus: &mut B, mode: u8, rm: u8) -> Exec<(u32, usize)> {
        let mut seg = DS;
        let mut base;
        if rm == 4 {
            let sib = self.fetch8(bus)?;
            let scale = sib >> 6;
            let index = (sib >> 3) & 7;
            let base_reg = sib & 7;
            if base_reg == 5 && mode == 0 {
                base = self.fetch32(bus)?;
            } else {
                base = self.r[base_reg as usize];
                if base_reg as usize == SP || base_reg as usize == BP {
                    seg = SS;
                }
            }
            if index != 4 {
                base = base.wrapping_add(self.r[index as usize] << scale);
            }
        } else if rm == 5 && mode == 0 {
            base = self.fetch32(bus)?;
        } else {
            base = self.r[rm as usize];
            if rm as usize == BP {
                seg = SS;
            }
        }
        let disp = match mode {
            1 => self.fetch8(bus)? as i8 as i32 as u32,
            2 => self.fetch32(bus)?,
            _ => 0,
        };
        Ok((base.wrapping_add(disp), seg))
    }

    #[inline(always)]
    pub(crate) fn rd_rm8<B: Bus>(&mut self, bus: &mut B, m: &ModRm) -> Exec<u8> {
        if m.is_reg {
            Ok(self.reg8(m.rm))
        } else {
            self.rd8(bus, m.seg as usize, m.off)
        }
    }

    #[inline(always)]
    pub(crate) fn wr_rm8<B: Bus>(&mut self, bus: &mut B, m: &ModRm, value: u8) -> Exec {
        if m.is_reg {
            self.set_reg8(m.rm, value);
            Ok(())
        } else {
            self.wr8(bus, m.seg as usize, m.off, value)
        }
    }

    #[inline(always)]
    pub(crate) fn rd_rmv<B: Bus>(&mut self, bus: &mut B, m: &ModRm, w: u32) -> Exec<u32> {
        if m.is_reg {
            Ok(self.regv(m.rm, w))
        } else {
            self.rdv(bus, m.seg as usize, m.off, w)
        }
    }

    #[inline(always)]
    pub(crate) fn wr_rmv<B: Bus>(&mut self, bus: &mut B, m: &ModRm, w: u32, value: u32) -> Exec {
        if m.is_reg {
            self.set_regv(m.rm, w, value);
            Ok(())
        } else {
            self.wrv(bus, m.seg as usize, m.off, w, value)
        }
    }

    /// Reads a register or memory operand of width 8, 16 or 32.
    #[inline(always)]
    pub(crate) fn rd_rmw<B: Bus>(&mut self, bus: &mut B, m: &ModRm, w: u32) -> Exec<u32> {
        if w == 8 {
            Ok(self.rd_rm8(bus, m)? as u32)
        } else {
            self.rd_rmv(bus, m, w)
        }
    }

    /// Writes a register or memory operand of width 8, 16 or 32.
    #[inline(always)]
    pub(crate) fn wr_rmw<B: Bus>(&mut self, bus: &mut B, m: &ModRm, w: u32, value: u32) -> Exec {
        if w == 8 {
            self.wr_rm8(bus, m, value as u8)
        } else {
            self.wr_rmv(bus, m, w, value)
        }
    }

    #[inline(always)]
    pub(crate) fn wrap_addr(&self, value: u32) -> u32 {
        if self.addr32 {
            value
        } else {
            value & 0xFFFF
        }
    }

    /// Sets the instruction pointer for a near branch at the operand size.
    #[inline(always)]
    pub(crate) fn branch(&mut self, target: u32) -> Exec {
        let target = if self.op32 { target } else { target & 0xFFFF };
        if self.pmode && target > self.seg[CS].hi {
            return Err(Trap::FaultCode(13, 0));
        }
        self.ip = target;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Execution
    // ------------------------------------------------------------------

    /// Executes one instruction, or one chunk of a `REP` instruction.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> Step {
        if self.halted {
            return Step::Halted;
        }

        let trap_after = self.eflags & FLAG_TF != 0 && !self.inhibit;
        self.inhibit = false;
        self.no_trap = false;
        self.start_ip = self.ip;
        self.start_sp = self.r[SP];
        let saved = Saved {
            eflags: self.eflags,
            lazy: self.lazy,
            lazy_res: self.lazy_res,
            lazy_cout: self.lazy_cout,
            lazy_bits: self.lazy_bits,
        };

        self.cycles = self.cycles.wrapping_add(1);
        let step = match self.exec_one(bus) {
            Ok(()) => Step::Ok,
            Err(Trap::Blocked) => {
                self.ip = self.start_ip;
                self.r[SP] = self.start_sp;
                self.restore_flags(&saved);
                // The BIOS waits with interrupts enabled.
                self.eflags |= FLAG_IF;
                return Step::Blocked;
            }
            Err(Trap::Terminated) => return Step::Halted,
            Err(Trap::Hlt) => {
                self.halted = true;
                return Step::Halted;
            }
            Err(Trap::Fault(vector)) => {
                self.ip = self.start_ip;
                self.r[SP] = self.start_sp;
                self.restore_flags(&saved);
                return self.exception(bus, vector, None);
            }
            Err(Trap::FaultCode(vector, code)) => {
                self.ip = self.start_ip;
                self.r[SP] = self.start_sp;
                self.restore_flags(&saved);
                return self.exception(bus, vector, Some(code));
            }
        };
        if trap_after && !self.no_trap {
            return self.exception(bus, 1, None);
        }
        step
    }

    fn restore_flags(&mut self, saved: &Saved) {
        self.eflags = saved.eflags;
        self.lazy = saved.lazy;
        self.lazy_res = saved.lazy_res;
        self.lazy_cout = saved.lazy_cout;
        self.lazy_bits = saved.lazy_bits;
    }

    /// Delivers a processor exception, escalating when delivery itself faults.
    fn exception<B: Bus>(&mut self, bus: &mut B, vector: u8, code: Option<u32>) -> Step {
        self.no_trap = true;
        let first = match self.interrupt(bus, vector, code, false) {
            Ok(()) => return Step::Ok,
            Err(trap) => trap,
        };
        let second = match first {
            Trap::Fault(vector) => (vector, None),
            Trap::FaultCode(vector, code) => (vector, Some(code)),
            _ => return Step::Fault(vector),
        };
        let contributory = |vector: u8| matches!(vector, 0 | 10 | 11 | 12 | 13);
        let double = vector == 14 && (second.0 == 14 || contributory(second.0))
            || contributory(vector) && contributory(second.0);
        let result = if vector == 8 {
            return Step::Fault(8);
        } else if double {
            self.interrupt(bus, 8, Some(0), false)
        } else {
            self.interrupt(bus, second.0, second.1, false)
        };
        match result {
            Ok(()) => Step::Ok,
            Err(_) => Step::Fault(vector),
        }
    }

    /// Delivers a hardware interrupt between instructions and wakes from `HLT`.
    pub fn hardware_interrupt<B: Bus>(&mut self, bus: &mut B, vector: u8) -> Step {
        self.halted = false;
        self.start_ip = self.ip;
        self.start_sp = self.r[SP];
        match self.interrupt(bus, vector, None, false) {
            Ok(()) => Step::Ok,
            Err(Trap::FaultCode(vector, code)) => self.exception(bus, vector, Some(code)),
            Err(Trap::Fault(vector)) => self.exception(bus, vector, None),
            Err(_) => Step::Ok,
        }
    }

    /// Enters an interrupt or exception handler at the given vector.
    pub(crate) fn interrupt<B: Bus>(
        &mut self,
        bus: &mut B,
        vector: u8,
        code: Option<u32>,
        software: bool,
    ) -> Exec {
        if self.pmode {
            return self.interrupt_protected(bus, vector, code, software);
        }
        let entry = (vector as u32) * 4;
        if entry + 3 > self.idtr.limit {
            return Err(Trap::FaultCode(13, 0));
        }
        let base = self.idtr.base.wrapping_add(entry);
        let ip = bus.read16(base);
        let cs = bus.read16(base.wrapping_add(2));
        let flags = self.flags() as u16;
        self.push16(bus, flags)?;
        let return_cs = self.sregs[CS];
        self.push16(bus, return_cs)?;
        let return_ip = self.ip as u16;
        self.push16(bus, return_ip)?;
        self.eflags &= !(FLAG_IF | FLAG_TF);
        self.set_real_segment(CS, cs);
        self.ip = ip as u32;
        Ok(())
    }

    /// Runs the service behind a callback instruction, then patches the flags
    /// it returns into the frame the stub's `IRET` will pop.
    fn callback<B: Bus>(&mut self, bus: &mut B, vector: u8, patch_flags: bool) -> Exec {
        self.materialize();
        bus.sync(self.cycles);
        let action = {
            let mut regs = CpuRegs {
                r: &mut self.r,
                sregs: &mut self.sregs,
                flags: &mut self.eflags,
            };
            bus.interrupt(vector, &mut regs)
        };
        self.cycles = self.cycles.wrapping_add(bus.take_cost() as u64);
        match action {
            InterruptAction::Blocked => return Err(Trap::Blocked),
            InterruptAction::Halt => return Err(Trap::Terminated),
            InterruptAction::Continue => {}
        }
        for index in [ES, SS, DS, FS, GS] {
            let selector = self.sregs[index];
            self.set_real_segment(index, selector);
        }
        if !patch_flags {
            return Ok(());
        }
        let frame = Self::physical(self.sregs[SS], (self.r[SP] as u16).wrapping_add(4));
        let mut stacked = bus.read16(frame) as u32;
        stacked &= !(FLAG_CF | FLAG_ZF);
        stacked |= self.eflags & (FLAG_CF | FLAG_ZF);
        bus.write16(frame, stacked as u16);
        Ok(())
    }

    fn exec_one<B: Bus>(&mut self, bus: &mut B) -> Exec {
        let big = self.seg[CS].big;
        self.op32 = big;
        self.addr32 = big;
        self.seg_override = None;
        self.rep = 0;
        let mut length = 0;
        let opcode = loop {
            let byte = self.fetch8(bus)?;
            match byte {
                0x26 => self.seg_override = Some(ES as u8),
                0x2E => self.seg_override = Some(CS as u8),
                0x36 => self.seg_override = Some(SS as u8),
                0x3E => self.seg_override = Some(DS as u8),
                0x64 => self.seg_override = Some(FS as u8),
                0x65 => self.seg_override = Some(GS as u8),
                0x66 => self.op32 = !big,
                0x67 => self.addr32 = !big,
                0xF0 => {}
                0xF2 | 0xF3 => self.rep = byte,
                _ => break byte,
            }
            length += 1;
            if length >= MAX_INSTRUCTION_LEN {
                return Err(Trap::Fault(6));
            }
        };

        #[cfg(feature = "trace")]
        {
            let slot = self.trace_index % TRACE_LEN;
            self.trace[slot] = TraceEntry {
                cs: self.sregs[CS],
                ip: self.start_ip,
                opcode,
                r: self.r,
                ds: self.sregs[DS],
                es: self.sregs[ES],
                flags: self.flags(),
            };
            self.trace_index = self.trace_index.wrapping_add(1);
        }

        ops::Ops::<B>::TABLE[opcode as usize](self, bus, opcode)
    }
}

/// The mask for an operand width.
#[inline(always)]
pub(crate) const fn mask(bits: u32) -> u32 {
    if bits >= 32 {
        0xFFFF_FFFF
    } else {
        (1u32 << bits) - 1
    }
}
