//! EL1 exception vectors and a diagnostic fault handler.

use core::arch::{asm, global_asm};

global_asm!(include_str!("vectors.S"));

extern "C" {
    static exception_vector_table: u8;
}

/// Points VBAR_EL1 at the table defined in `vectors.S`.
pub fn init() {
    let table = unsafe { &exception_vector_table as *const u8 as usize };

    unsafe {
        asm!("msr vbar_el1, {}", in(reg) table, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
}

/// Called from `exception_common` with the vector index and key system
/// registers. There is no recovery path in this skeleton, so we report and
/// halt.
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
    let ec = (esr >> 26) & 0x3f;

    crate::console::_print_unlocked(format_args!(
        "\n[exception] {} / {} (vector {index})\n\
         [exception] ESR_EL1={esr:#018x} EC={ec:#04x} ({})\n\
         [exception] ELR_EL1={elr:#018x} FAR_EL1={far:#018x}\n\
         [exception] SPSR_EL1={spsr:#018x} SP={sp:#018x}\n\
         [exception] system halted\n",
        vector_group(index),
        vector_type(index),
        syndrome_name(ec),
    ));

    crate::halt()
}

fn vector_group(index: u64) -> &'static str {
    match index / 4 {
        0 => "current EL with SP0",
        1 => "current EL with SPx",
        2 => "lower EL (AArch64)",
        _ => "lower EL (AArch32)",
    }
}

fn vector_type(index: u64) -> &'static str {
    match index % 4 {
        0 => "synchronous",
        1 => "IRQ",
        2 => "FIQ",
        _ => "SError",
    }
}

fn syndrome_name(ec: u64) -> &'static str {
    match ec {
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
