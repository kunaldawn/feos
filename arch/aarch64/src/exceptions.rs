//! EL1 exception vectors and fault reporting.
//!
//! The assembly table in `vectors.S` routes every entry through
//! `irq::trap_dispatch`, which sends anything that isn't an IRQ or a software
//! switch here. [`fault`] builds a [`FaultInfo`] and forwards it to the handler
//! registered with [`set_handler`]. The handler may end the faulting task, and
//! otherwise reports the fault and stops the machine.

use core::arch::{asm, global_asm};
use core::fmt;
use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::cpu;
use crate::irq::TrapFrame;

global_asm!(include_str!("vectors.S"));

extern "C" {
    static exception_vector_table: u8;
}

/// State captured when an exception is taken.
#[derive(Clone, Copy, Debug)]
pub struct FaultInfo {
    /// Vector index 0..=15.
    pub index: u64,
    /// `ESR_EL1`.
    pub esr: u64,
    /// `ELR_EL1`.
    pub elr: u64,
    /// `FAR_EL1`.
    pub far: u64,
    /// `SPSR_EL1`.
    pub spsr: u64,
    /// Stack pointer at the time of the fault.
    pub sp: u64,
    /// `x0`..`x30` at the time of the fault.
    pub regs: [u64; 31],
}

/// Vector index of a synchronous exception from a thread.
const VECTOR_THREAD_SYNC: u64 = 0;
/// Vector index of a synchronous exception from a user program.
const VECTOR_USER_SYNC: u64 = 8;
/// `ESR_EL1` classes of an instruction or data abort from the current level.
const CLASS_INSTRUCTION_ABORT: u64 = 0x21;
const CLASS_DATA_ABORT: u64 = 0x25;
/// The same from a lower level.
const CLASS_USER_INSTRUCTION_ABORT: u64 = 0x20;
const CLASS_USER_DATA_ABORT: u64 = 0x24;

impl FaultInfo {
    /// Whether this is something a thread did, as opposed to the machine failing,
    /// so that ending the task could be an answer to it. An SError or anything from
    /// the exception handlers themselves isn't.
    pub const fn is_task_fault(&self) -> bool {
        self.index == VECTOR_THREAD_SYNC || self.index == VECTOR_USER_SYNC
    }

    /// Whether a user program caused this.
    pub const fn from_user(&self) -> bool {
        self.index == VECTOR_USER_SYNC
    }

    /// The address an abort was on, if this is one.
    pub const fn fault_address(&self) -> Option<u64> {
        match self.exception_class() {
            CLASS_INSTRUCTION_ABORT
            | CLASS_DATA_ABORT
            | CLASS_USER_INSTRUCTION_ABORT
            | CLASS_USER_DATA_ABORT => Some(self.far),
            _ => None,
        }
    }

    /// A translation, access flag or permission fault from EL0, which a change to the
    /// tables can answer.
    pub const fn memory_fault(&self) -> Option<feos_hal::MemoryFault> {
        let class = self.exception_class();
        if class != CLASS_USER_INSTRUCTION_ABORT && class != CLASS_USER_DATA_ABORT {
            return None;
        }
        // The fault status code's top bits: 0b0001 translation, 0b0010 access flag,
        // 0b0011 permission, at any level.
        if !matches!((self.esr & 0x3f) >> 2, 0b0001..=0b0011) {
            return None;
        }
        Some(feos_hal::MemoryFault {
            address: self.far,
            // `WnR`, for a data abort; cache maintenance counts as a write, as it should.
            write: class == CLASS_USER_DATA_ABORT && self.esr & (1 << 6) != 0,
            execute: class == CLASS_USER_INSTRUCTION_ABORT,
        })
    }

    /// The signal a fault from EL0 raises, and its code (section 8.2 of the ABI design).
    /// Integer division by zero raises nothing on AArch64, so there's no `SIGFPE` but a
    /// trapped floating-point exception, which nothing enables.
    pub const fn user_signal(&self) -> (u32, i32) {
        use feos_abi::{si, sig};
        match self.exception_class() {
            CLASS_USER_INSTRUCTION_ABORT | CLASS_USER_DATA_ABORT => {
                // Alignment faults (status 0b100001) are the bus's; anything else the
                // tables couldn't answer is a bad address.
                if self.esr & 0x3f == 0b10_0001 {
                    (sig::BUS, si::BUS_ADRALN)
                } else {
                    (sig::SEGV, si::SEGV_MAPERR)
                }
            }
            // PC and SP alignment.
            0x22 | 0x26 => (sig::BUS, si::BUS_ADRALN),
            // `brk`, and breakpoints, steps and watchpoints from EL0.
            0x3c | 0x30 | 0x32 | 0x34 => (sig::TRAP, si::TRAP_BRKPT),
            // A trapped floating-point exception.
            0x2c => (sig::FPE, 0),
            // Unknown and undefined instructions, trapped system register accesses, `wfi`,
            // an illegal execution state, and whatever else.
            _ => (sig::ILL, si::ILL_ILLOPC),
        }
    }

    /// Writes the general-purpose registers, four to a line.
    pub fn write_registers(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        for (index, value) in self.regs.iter().enumerate() {
            let separator = if index % 4 == 3 || index == self.regs.len() - 1 {
                "\n"
            } else {
                " "
            };
            write!(out, "x{index:<2}={value:016x}{separator}")?;
        }
        Ok(())
    }

    /// Exception class extracted from `ESR_EL1`.
    pub const fn exception_class(&self) -> u64 {
        (self.esr >> 26) & 0x3f
    }

    /// Human-readable vector group.
    pub const fn vector_group(&self) -> &'static str {
        match self.index / 4 {
            0 => "current EL with SP0",
            1 => "current EL with SPx",
            2 => "lower EL (AArch64)",
            _ => "lower EL (AArch32)",
        }
    }

    /// Human-readable vector type.
    pub const fn vector_type(&self) -> &'static str {
        match self.index % 4 {
            0 => "synchronous",
            1 => "IRQ",
            2 => "FIQ",
            _ => "SError",
        }
    }

    /// Human-readable name for the exception class.
    pub const fn syndrome_name(&self) -> &'static str {
        match self.exception_class() {
            0x00 => "unknown reason",
            0x01 => "trapped WFI/WFE",
            0x03 => "trapped MCR/MRC",
            0x07 => "trapped SVE/SIMD/FP",
            0x0e => "illegal execution state",
            0x15 => "SVC from AArch64",
            0x16 => "HVC from AArch64",
            0x17 => "SMC from AArch64",
            0x18 => "trapped MSR/MRS",
            0x20 => "instruction abort from lower EL",
            0x21 => "instruction abort from same EL",
            0x22 => "PC alignment fault",
            0x24 => "data abort from lower EL",
            0x25 => "data abort from same EL",
            0x26 => "SP alignment fault",
            0x2c => "trapped floating-point",
            0x32 | 0x33 => "breakpoint",
            0x34 | 0x35 => "software step",
            0x38 | 0x39 => "watchpoint",
            0x3c => "BRK instruction",
            _ => "unhandled",
        }
    }
}

/// Callback invoked for every exception.
/// It can replace the frame with another task's and return `true` to resume that,
/// rather than stopping the machine.
pub type FaultHandler = fn(&FaultInfo, &mut TrapFrame) -> bool;

static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Points `VBAR_EL1` at the table defined in `vectors.S`.
pub fn init() {
    let table = &raw const exception_vector_table as usize;

    // SAFETY: the table is 2 KiB aligned and lives in `.text.vectors`.
    unsafe {
        asm!("msr vbar_el1, {}", in(reg) table, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
}

/// Installs the function called when an exception is taken.
///
/// The architecture crate halts if the handler returns.
pub fn set_handler(handler: FaultHandler) {
    HANDLER.store(handler as usize, Ordering::Release);
}

fn handler() -> Option<FaultHandler> {
    let address = HANDLER.load(Ordering::Acquire);
    if address == 0 {
        None
    } else {
        // SAFETY: only `set_handler` writes this atomic, and it stores a valid
        // `FaultHandler` (a plain function pointer is `usize`-sized).
        Some(unsafe { transmute::<usize, FaultHandler>(address) })
    }
}

/// Reports a trap that isn't an interrupt or a request. Called from
/// `irq::trap_dispatch`. Doesn't return unless the handler has dealt with it.
pub(crate) fn fault(index: u64, esr: u64, frame: &mut TrapFrame) {
    let elr: u64;
    let far: u64;
    // SAFETY: reading system registers has no side effects.
    unsafe {
        asm!("mrs {}, elr_el1", out(reg) elr, options(nomem, nostack));
        asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack));
    }
    let info = FaultInfo {
        index,
        esr,
        elr,
        far,
        spsr: frame.spsr,
        sp: frame.sp,
        regs: frame.x,
    };

    if let Some(handler) = handler() {
        if handler(&info, frame) {
            return;
        }
    }

    cpu::halt()
}
