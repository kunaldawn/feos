//! EL1 exception vectors and fault reporting.
//!
//! The assembly table in `vectors.S` routes every entry through
//! `exception_handler`, which builds a [`FaultInfo`] and forwards it to the
//! handler registered with [`set_handler`]. The handler reports the fault; it
//! cannot return, because there is no recovery path yet.

use core::arch::{asm, global_asm};
use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::cpu;

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
}

impl FaultInfo {
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
pub type FaultHandler = fn(&FaultInfo);

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
/// The handler must not return; the architecture crate halts after it runs.
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

/// Called from `exception_common` in `vectors.S`.
///
/// # Safety
/// Only called from the assembly exception entry stub.
#[no_mangle]
pub unsafe extern "C" fn exception_handler(
    index: u64,
    esr: u64,
    elr: u64,
    far: u64,
    spsr: u64,
    sp: u64,
) -> ! {
    let info = FaultInfo {
        index,
        esr,
        elr,
        far,
        spsr,
        sp,
    };

    if let Some(handler) = handler() {
        handler(&info);
    }

    cpu::halt()
}
