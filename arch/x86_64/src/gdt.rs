//! The kernel's GDT and TSS, and the per-CPU stacks they provide.
//!
//! `boot.S` loads a temporary GDT to enter long mode. This one replaces it and
//! adds a TSS per CPU. The kernel never leaves ring 0, so the TSS exists only for
//! its interrupt stack table: an IDT gate that names an entry switches to that
//! stack in hardware, which is the only way to leave the interrupted stack on a
//! same-privilege interrupt.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr::{addr_of, addr_of_mut};

use crate::cpu;
use crate::smp::MAX_CPUS;

/// IST entry for hardware interrupts, IPIs and the software switch.
pub const IST_IRQ: u8 = 1;
/// IST entry for CPU exceptions.
///
/// Faults are separate from IRQs so a fault inside the IRQ path can't overwrite
/// the frame it interrupted, which would hide what went wrong.
pub const IST_FAULT: u8 = 2;

const IRQ_STACK_SIZE: usize = 32 * 1024;
const FAULT_STACK_SIZE: usize = 16 * 1024;

/// Selector of the first TSS descriptor. Each takes two GDT slots.
const TSS_SELECTOR: u16 = 0x20;
const GDT_ENTRIES: usize = 4 + 2 * MAX_CPUS;

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

const _: () = assert!(size_of::<Tss>() == 104);

#[repr(C, align(16))]
struct CpuStacks {
    irq: UnsafeCell<[u8; IRQ_STACK_SIZE]>,
    fault: UnsafeCell<[u8; FAULT_STACK_SIZE]>,
}

// SAFETY: each CPU only uses its own stacks, and only through the CPU itself.
unsafe impl Sync for CpuStacks {}

static STACKS: [CpuStacks; MAX_CPUS] = [const {
    CpuStacks {
        irq: UnsafeCell::new([0; IRQ_STACK_SIZE]),
        fault: UnsafeCell::new([0; FAULT_STACK_SIZE]),
    }
}; MAX_CPUS];

static mut TSS: [Tss; MAX_CPUS] = [Tss {
    reserved0: 0,
    rsp: [0; 3],
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    // No I/O permission bitmap: the base points past the segment limit.
    iomap_base: size_of::<Tss>() as u16,
}; MAX_CPUS];

/// Null, 64-bit code, flat data, 32-bit code, then two slots per TSS. It must be
/// writable because `ltr` sets the descriptor's busy bit.
static mut GDT: [u64; GDT_ENTRIES] = {
    let mut gdt = [0; GDT_ENTRIES];
    gdt[1] = 0x00AF_9A00_0000_FFFF;
    gdt[2] = 0x00CF_9200_0000_FFFF;
    gdt[3] = 0x00CF_9A00_0000_FFFF;
    gdt
};

#[repr(C, packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

/// Encodes an available 64-bit TSS descriptor as its two GDT slots.
const fn tss_descriptor(base: u64, limit: u64) -> [u64; 2] {
    let low = (limit & 0xffff)
        | ((base & 0xff_ffff) << 16)
        | (0x89 << 40)
        | ((limit & 0xf_0000) << 32)
        | ((base & 0xff00_0000) << 32);
    [low, base >> 32]
}

/// Loads the GDT and this CPU's TSS. Call before installing the IDT.
pub fn init() {
    let index = cpu::index().min(MAX_CPUS - 1);
    let stacks = &STACKS[index];
    let irq_top = stacks.irq.get() as u64 + IRQ_STACK_SIZE as u64;
    let fault_top = stacks.fault.get() as u64 + FAULT_STACK_SIZE as u64;

    // SAFETY: boot runs this once per CPU with interrupts masked, and each CPU
    // only writes its own TSS and descriptor.
    unsafe {
        let tss = addr_of_mut!(TSS[index]);
        (*tss).ist[IST_IRQ as usize - 1] = irq_top;
        (*tss).ist[IST_FAULT as usize - 1] = fault_top;

        let [low, high] = tss_descriptor(tss as u64, size_of::<Tss>() as u64 - 1);
        let gdt = addr_of_mut!(GDT).cast::<u64>();
        gdt.add(4 + 2 * index).write(low);
        gdt.add(5 + 2 * index).write(high);

        let pointer = GdtPointer {
            limit: (size_of::<u64>() * GDT_ENTRIES - 1) as u16,
            base: addr_of!(GDT) as u64,
        };
        asm!(
            // The selectors are unchanged, so the segment registers stay valid.
            "lgdt [{pointer}]",
            "ltr {selector:x}",
            pointer = in(reg) &pointer,
            selector = in(reg) TSS_SELECTOR + 16 * index as u16,
            options(nostack),
        );
    }
}
