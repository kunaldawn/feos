//! Interrupt descriptor table and fault reporting.
//!
//! The IDT points vectors 0..50 at the stubs in `interrupts.S`. Exceptions
//! (vectors 0..31) build a [`FaultInfo`] and forward it to the handler
//! registered with [`set_handler`], then halt. Hardware interrupts, the software
//! switch and IPIs (vectors 32..50) go through the recoverable path in
//! [`crate::irq`] instead.
//!
//! Each gate names a per-CPU stack from the TSS, so a fault still has a working
//! stack when the task's own stack pointer is bad.

use core::arch::asm;
use core::fmt;
use core::mem::transmute;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{cpu, gdt};

/// State captured when an exception is taken.
#[derive(Clone, Copy, Debug)]
pub struct FaultInfo {
    /// Interrupt vector 0..31.
    pub index: u64,
    /// CPU error code (`0` when the vector has none).
    pub esr: u64,
    /// Interrupted instruction pointer.
    pub elr: u64,
    /// Faulting address (`CR2` for page faults, otherwise `0`).
    pub far: u64,
    /// Interrupted flags.
    pub spsr: u64,
    /// Interrupted stack pointer.
    pub sp: u64,
    /// General-purpose registers except `rsp`, named by [`REGISTER_NAMES`].
    pub regs: [u64; 15],
}

/// Names of [`FaultInfo::regs`], in order.
pub const REGISTER_NAMES: [&str; 15] = [
    "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "r8", "r9", "r10", "r11", "r12", "r13", "r14",
    "r15",
];

impl FaultInfo {
    /// Writes the general-purpose registers, four to a line.
    pub fn write_registers(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        for (index, (name, value)) in REGISTER_NAMES.iter().zip(self.regs).enumerate() {
            let separator = if index % 4 == 3 || index == self.regs.len() - 1 {
                "\n"
            } else {
                " "
            };
            write!(out, "{name:<3}={value:016x}{separator}")?;
        }
        Ok(())
    }

    /// Exception class. On x86 this is the vector itself.
    pub const fn exception_class(&self) -> u64 {
        self.index
    }

    /// Human-readable vector group.
    pub const fn vector_group(&self) -> &'static str {
        "x86_64"
    }

    /// Human-readable vector type.
    pub const fn vector_type(&self) -> &'static str {
        match self.index {
            8 | 17 | 18 => "abort",
            1 | 3 | 4 => "trap",
            _ => "fault",
        }
    }

    /// Human-readable name for the exception vector.
    pub const fn syndrome_name(&self) -> &'static str {
        match self.index {
            0 => "divide error",
            1 => "debug",
            2 => "non-maskable interrupt",
            3 => "breakpoint",
            4 => "overflow",
            5 => "bound range exceeded",
            6 => "invalid opcode",
            7 => "device not available",
            8 => "double fault",
            9 => "coprocessor segment overrun",
            10 => "invalid TSS",
            11 => "segment not present",
            12 => "stack-segment fault",
            13 => "general protection fault",
            14 => "page fault",
            15 => "reserved",
            16 => "x87 floating-point exception",
            17 => "alignment check",
            18 => "machine check",
            19 => "SIMD floating-point exception",
            20 => "virtualization exception",
            21 => "control protection exception",
            28 => "hypervisor injection exception",
            29 => "VMM communication exception",
            30 => "security exception",
            31 => "reserved",
            _ => "unknown",
        }
    }
}

/// Callback invoked for every exception.
pub type FaultHandler = fn(&FaultInfo);

static HANDLER: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

impl IdtEntry {
    const MISSING: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    };

    fn new(handler: u64, selector: u16, type_attr: u8, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector,
            ist,
            type_attr,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            zero: 0,
        }
    }
}

const _: () = assert!(core::mem::size_of::<IdtEntry>() == 16);

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::MISSING; 256];

/// Vectors with a stub in `interrupts.S`.
const STUBS: usize = 51;

extern "C" {
    static isr_stub_table: [u64; STUBS];
    static isr_spurious: u64;
}

/// Builds and loads the IDT.
pub fn init() {
    // SAFETY: boot calls this once, before interrupts are unmasked. The table
    // is a private static and every entry is written below.
    let table = unsafe { &mut *addr_of_mut!(IDT) };

    for (index, entry) in table.iter_mut().enumerate().take(STUBS) {
        // SAFETY: the assembly table has exactly `STUBS` entries.
        let handler = unsafe { isr_stub_table[index] };
        let stack = if index < 32 { gdt::IST_FAULT } else { gdt::IST_IRQ };
        *entry = IdtEntry::new(handler, 0x08, 0x8e, stack);
    }

    let spurious = addr_of!(isr_spurious) as u64;
    for entry in table.iter_mut().skip(STUBS) {
        *entry = IdtEntry::new(spurious, 0x08, 0x8e, gdt::IST_IRQ);
    }

    let pointer = IdtPointer {
        limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
        base: table.as_ptr() as u64,
    };

    // SAFETY: `pointer` describes the static table and `lidt` reads it.
    unsafe {
        asm!("lidt [{}]", in(reg) &pointer, options(nostack));
    }
}

/// Installs the function called when an exception is taken.
pub fn set_handler(handler: FaultHandler) {
    HANDLER.store(handler as usize, Ordering::Release);
}

fn handler() -> Option<FaultHandler> {
    let address = HANDLER.load(Ordering::Acquire);
    if address == 0 {
        None
    } else {
        // SAFETY: only `set_handler` writes this atomic, and a function
        // pointer is `usize`-sized on this target.
        Some(unsafe { transmute::<usize, FaultHandler>(address) })
    }
}

fn cr2() -> u64 {
    let value: u64;
    // SAFETY: reading CR2 has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack)) };
    value
}

/// Called from the recoverable interrupt dispatch for vectors 0..31.
pub(crate) fn handle(frame: &crate::irq::TrapFrame) {
    let info = FaultInfo {
        index: frame.vector,
        esr: frame.error,
        elr: frame.rip,
        far: if frame.vector == 14 { cr2() } else { 0 },
        spsr: frame.rflags,
        sp: frame.rsp,
        regs: [
            frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rsi, frame.rdi, frame.rbp, frame.r8,
            frame.r9, frame.r10, frame.r11, frame.r12, frame.r13, frame.r14, frame.r15,
        ],
    };

    if let Some(handler) = handler() {
        handler(&info);
    }

    cpu::halt()
}
