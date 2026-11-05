//! Interrupt descriptor table and fault reporting.
//!
//! The IDT points every vector at the stubs in `interrupts.S`. Exceptions
//! (vectors 0..31) build a [`FaultInfo`] and forward it to the handler
//! registered with [`set_handler`], which may stop the machine or end the task.
//! Hardware interrupts, the software
//! switch and IPIs go through the recoverable path in [`crate::irq`] instead.
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
    /// Interrupted code segment, whose low bits are the privilege it ran at.
    pub cs: u64,
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

    /// Whether this is something a task did, as opposed to the machine failing, so
    /// that ending the task could be an answer to it.
    pub const fn is_task_fault(&self) -> bool {
        // Divide, debug, breakpoint, overflow, bound, opcode, device not available,
        // stack, protection, page, x87, alignment and SIMD. Not NMI, double fault
        // or machine check.
        //
        // From ring 3, anything but those last ones is the program's doing: a bad
        // segment selector can raise others.
        if self.from_user() {
            return !matches!(self.index, 2 | 8 | 18);
        }
        matches!(self.index, 0 | 1 | 3 | 4 | 5 | 6 | 7 | 12 | 13 | 14 | 16 | 17 | 19)
    }

    /// Whether a user program caused this.
    pub const fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }

    /// The address a page fault was on, if this is one.
    pub const fn fault_address(&self) -> Option<u64> {
        if self.index == 14 {
            Some(self.far)
        } else {
            None
        }
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

/// Callback invoked for every exception. It can replace the frame with another
/// task's and return `true` to resume that, rather than stopping the machine.
pub type FaultHandler = fn(&FaultInfo, &mut crate::irq::TrapFrame) -> bool;

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

/// Exception stubs, listed in `isr_stub_table`.
const EXCEPTIONS: usize = 32;
/// Bytes per stub from `isr_irq_stubs` on.
const IRQ_STUB_SIZE: usize = 16;
/// The local APIC's spurious vector, which must not be acknowledged.
const SPURIOUS_VECTOR: usize = 255;

extern "C" {
    static isr_stub_table: [u64; EXCEPTIONS];
    static isr_irq_stubs: u8;
    static isr_spurious: u8;
}

/// Builds and loads the IDT.
pub fn init() {
    // SAFETY: boot calls this once, before interrupts are unmasked. The table
    // is a private static and every entry is written below.
    let table = unsafe { &mut *addr_of_mut!(IDT) };

    for (vector, entry) in table.iter_mut().enumerate() {
        let (handler, stack) = if vector < EXCEPTIONS {
            // SAFETY: the assembly table has exactly `EXCEPTIONS` entries.
            (unsafe { isr_stub_table[vector] }, gdt::IST_FAULT)
        } else if vector == SPURIOUS_VECTOR {
            (addr_of!(isr_spurious) as u64, gdt::IST_IRQ)
        } else {
            let stubs = addr_of!(isr_irq_stubs) as u64;
            (stubs + ((vector - EXCEPTIONS) * IRQ_STUB_SIZE) as u64, gdt::IST_IRQ)
        };
        // Present and an interrupt gate. Only the system call's may be raised from
        // ring 3: `int` to any other gate is a protection fault.
        let attributes = if vector as u64 == crate::irq::SYSCALL_VECTOR { 0xee } else { 0x8e };
        *entry = IdtEntry::new(handler, 0x08, attributes, stack);
    }

    load();
}

/// Loads the IDT on this CPU. [`init`] has already built it.
pub fn load() {
    let pointer = IdtPointer {
        limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
        base: addr_of!(IDT) as u64,
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
pub(crate) fn handle(frame: &mut crate::irq::TrapFrame) {
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
        cs: frame.cs,
    };

    if let Some(handler) = handler() {
        if handler(&info, frame) {
            return;
        }
    }

    cpu::halt()
}
