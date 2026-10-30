//! Recoverable IRQ entry and the saved register frame used by the scheduler.
//!
//! The vector table in `vectors.S` sends IRQs taken from EL1 with `SP_EL1` to
//! `irq_entry`. That stub saves every general-purpose and SIMD register on the
//! interrupted stack, asks the registered handler to service the interrupt,
//! and restores whatever frame the handler leaves behind. The scheduler uses
//! that indirection to replace the frame with another process's saved context,
//! so returning from the exception resumes a different process.
//!
//! Synchronous faults and exceptions from other vector groups still go through
//! [`crate::exceptions`] and halt the machine.

use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Saved CPU state at the moment an IRQ was taken.
///
/// The layout is part of the contract with `irq_entry` in `vectors.S`: it is
/// 800 bytes, 16-byte aligned, with the exact offsets documented below.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    /// `x0`..`x30`, at offsets 0..240.
    pub x: [u64; 31],
    /// Padding so the SIMD registers start at a 16-byte boundary (offset 248).
    pub _reserved: u64,
    /// `q0`..`q31`, at offsets 256..768.
    pub q: [u128; 32],
    /// Stack pointer of the interrupted context (offset 768).
    pub sp: u64,
    /// `ELR_EL1` of the interrupted context (offset 776).
    pub elr: u64,
    /// `SPSR_EL1` of the interrupted context (offset 784).
    pub spsr: u64,
    /// Padding so the frame is a multiple of 16 bytes (offset 792).
    pub _reserved2: u64,
}

// The layout is a hard contract with `irq_entry` in `vectors.S`.
const _: () = assert!(core::mem::size_of::<TrapFrame>() == 800);
const _: () = assert!(core::mem::align_of::<TrapFrame>() == 16);

impl TrapFrame {
    /// A frame with every register cleared.
    pub const ZERO: Self = Self {
        x: [0; 31],
        _reserved: 0,
        q: [0; 32],
        sp: 0,
        elr: 0,
        spsr: 0,
        _reserved2: 0,
    };

    /// Builds the initial frame for a process that has never run.
    ///
    /// `entry` is called with `arg` in `x0`, `stack_top` becomes the initial
    /// stack pointer, and a return from `entry` jumps to `lr`. `SPSR` selects
    /// EL1h with IRQs unmasked, so the new process is immediately preemptible.
    pub fn new_task(entry: usize, arg: usize, stack_top: usize, lr: usize) -> Self {
        debug_assert_eq!(stack_top & 0xf, 0, "process stack must be 16-byte aligned");

        let mut frame = Self::ZERO;
        frame.x[0] = arg as u64;
        frame.x[30] = lr as u64;
        frame.sp = stack_top as u64;
        frame.elr = entry as u64;
        // M[3:0] = 0b0101 (EL1h); DAIF bits clear => IRQs enabled.
        frame.spsr = 0b0101;
        frame
    }
}

/// Callback invoked with the frame of the interrupted process.
///
/// The handler may modify the frame; the stub restores it before returning.
pub type IrqHandler = fn(&mut TrapFrame);

static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Installs the function called for every IRQ from EL1.
pub fn set_handler(handler: IrqHandler) {
    HANDLER.store(handler as usize, Ordering::Release);
}

fn handler() -> Option<IrqHandler> {
    let address = HANDLER.load(Ordering::Acquire);
    if address == 0 {
        None
    } else {
        // SAFETY: only `set_handler` writes this atomic, and it stores a valid
        // `IrqHandler` (a plain function pointer is `usize`-sized).
        Some(unsafe { transmute::<usize, IrqHandler>(address) })
    }
}

/// Called from `irq_entry` in `vectors.S`.
///
/// # Safety
/// Only called from the assembly IRQ entry stub with a valid frame pointer.
#[no_mangle]
pub unsafe extern "C" fn irq_dispatch(frame: *mut TrapFrame) {
    if let Some(handler) = handler() {
        handler(&mut *frame);
    }
}

/// Unmasks IRQs (`DAIF.I = 0`).
pub fn enable() {
    // SAFETY: changing the interrupt mask has no memory side effects.
    unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags)) }
}

/// Masks IRQs (`DAIF.I = 1`).
pub fn disable() {
    // SAFETY: changing the interrupt mask has no memory side effects.
    unsafe { core::arch::asm!("msr daifset, #2", options(nomem, nostack, preserves_flags)) }
}

/// Masks IRQs and returns the previous `DAIF` value.
///
/// Pass the result to [`restore`] to re-enable interrupts only if they were
/// enabled before the critical section.
pub fn save_and_disable() -> u64 {
    let flags: u64;
    // SAFETY: reading DAIF and masking IRQs have no memory side effects.
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) flags, options(nomem, nostack));
        core::arch::asm!("msr daifset, #2", options(nomem, nostack, preserves_flags));
    }
    flags
}

/// Restores a `DAIF` value captured by [`save_and_disable`].
pub fn restore(flags: u64) {
    // SAFETY: `flags` comes from `save_and_disable`, which read DAIF.
    unsafe { core::arch::asm!("msr daif, {}", in(reg) flags, options(nomem, nostack)) }
}

/// Waits for an interrupt or other wake-up event.
pub fn wait_for_interrupt() {
    // SAFETY: `wfi` is available at EL1 and has no memory side effects.
    unsafe { core::arch::asm!("wfi", options(nomem, nostack)) }
}
