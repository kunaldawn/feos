//! Recoverable traps and the saved register frame used by the scheduler.
//!
//! The vector table in `vectors.S` saves every general-purpose and SIMD register
//! on this CPU's private exception stack and calls [`trap_dispatch`], which asks
//! the registered handler to service the trap. The stub then restores whatever
//! frame the handler leaves behind. The scheduler uses that indirection to
//! replace the frame with another process's saved context, so returning from the
//! exception resumes a different process.
//!
//! Threads run on `SP_EL0`, so hardware-selected `SP_EL1` is always a stack the
//! interrupted thread doesn't own. Once the scheduler has offered a task to other
//! CPUs, this CPU no longer touches that task's stack.
//!
//! Faults and exceptions from other vector groups go through
//! [`crate::exceptions`], which doesn't return.

use core::arch::asm;
use core::mem::{offset_of, transmute};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::exceptions;

/// Vector index of an IRQ taken from a thread (current EL with `SP_EL0`).
const VECTOR_THREAD_IRQ: u64 = 1;
/// Vector index of a synchronous exception taken from a thread.
const VECTOR_THREAD_SYNC: u64 = 0;
/// `ESR_EL1` exception class for `SVC` from AArch64.
const ESR_CLASS_SVC: u64 = 0x15;

/// Saved CPU state at the moment an IRQ was taken.
///
/// The layout is part of the contract with `trap_common` in `vectors.S`: it is
/// 800 bytes, 16-byte aligned, with the exact offsets documented below.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    /// `x0`..`x30`, at offsets 0..240.
    pub x: [u64; 31],
    /// `FPCR`, per task (offset 248).
    pub fpcr: u64,
    /// `q0`..`q31`, at offsets 256..768.
    pub q: [u128; 32],
    /// Stack pointer of the interrupted context (offset 768).
    pub sp: u64,
    /// `ELR_EL1` of the interrupted context (offset 776).
    pub elr: u64,
    /// `SPSR_EL1` of the interrupted context (offset 784).
    pub spsr: u64,
    /// `FPSR`, per task (offset 792).
    pub fpsr: u64,
}

// The layout is a hard contract with `trap_common` in `vectors.S`.
const _: () = assert!(core::mem::size_of::<TrapFrame>() == 800);
const _: () = assert!(core::mem::align_of::<TrapFrame>() == 16);
const _: () = assert!(offset_of!(TrapFrame, x) == 0);
const _: () = assert!(offset_of!(TrapFrame, fpcr) == 248);
const _: () = assert!(offset_of!(TrapFrame, q) == 256);
const _: () = assert!(offset_of!(TrapFrame, sp) == 768);
const _: () = assert!(offset_of!(TrapFrame, elr) == 776);
const _: () = assert!(offset_of!(TrapFrame, spsr) == 784);
const _: () = assert!(offset_of!(TrapFrame, fpsr) == 792);

/// `SPSR_EL1` for a new thread: EL1t, with only IRQs unmasked.
///
/// This matches the boot thread, which keeps the reset state of the D, A and F
/// masks and only ever clears I.
const SPSR_THREAD: u64 = 0b0100 | (1 << 9) | (1 << 8) | (1 << 6);

impl TrapFrame {
    /// A frame with every register cleared.
    pub const ZERO: Self = Self {
        x: [0; 31],
        fpcr: 0,
        q: [0; 32],
        sp: 0,
        elr: 0,
        spsr: 0,
        fpsr: 0,
    };

    /// Builds the initial frame for a process that has never run.
    ///
    /// `entry` is called with `arg` in `x0`, `stack_top` becomes the initial
    /// stack pointer, and a return from `entry` jumps to `lr`. `SPSR` selects
    /// EL1t with IRQs unmasked, so the new process is immediately preemptible.
    pub fn new_task(entry: usize, arg: usize, stack_top: usize, lr: usize) -> Self {
        debug_assert_eq!(stack_top & 0xf, 0, "process stack must be 16-byte aligned");

        let mut frame = Self::ZERO;
        frame.x[0] = arg as u64;
        frame.x[30] = lr as u64;
        frame.sp = stack_top as u64;
        frame.elr = entry as u64;
        frame.spsr = SPSR_THREAD;
        frame
    }

    /// Stack pointer of the interrupted context.
    pub fn stack_pointer(&self) -> usize {
        self.sp as usize
    }
}

/// Callback invoked with the frame of the interrupted process.
///
/// The handler may modify the frame; the stub restores it before returning.
pub type IrqHandler = fn(&mut TrapFrame);

static HANDLER: AtomicUsize = AtomicUsize::new(0);
static SWITCH_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Installs the function called for every IRQ from EL1.
pub fn set_handler(handler: IrqHandler) {
    HANDLER.store(handler as usize, Ordering::Release);
}

/// Installs the function called when a thread uses [`request_switch`].
pub fn set_switch_handler(handler: IrqHandler) {
    SWITCH_HANDLER.store(handler as usize, Ordering::Release);
}

fn load_handler(slot: &AtomicUsize) -> Option<IrqHandler> {
    let address = slot.load(Ordering::Acquire);
    if address == 0 {
        None
    } else {
        // SAFETY: only the setters above write these atomics, and they store a
        // valid `IrqHandler` (a plain function pointer is `usize`-sized).
        Some(unsafe { transmute::<usize, IrqHandler>(address) })
    }
}

/// Called from `trap_common` in `vectors.S`.
///
/// Returns only for an IRQ or a software switch from a thread. Everything else
/// is fatal.
///
/// # Safety
/// Only called from the assembly trap stub with a valid frame pointer.
#[no_mangle]
unsafe extern "C" fn trap_dispatch(index: u64, frame: *mut TrapFrame) {
    // SAFETY: the stub passes the frame it just saved on this CPU's stack.
    let frame = unsafe { &mut *frame };
    let esr: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, esr_el1", out(reg) esr, options(nomem, nostack)) };

    let handler = match index {
        VECTOR_THREAD_IRQ => &HANDLER,
        VECTOR_THREAD_SYNC if (esr >> 26) & 0x3f == ESR_CLASS_SVC => &SWITCH_HANDLER,
        _ => exceptions::fatal(index, esr, frame),
    };
    if let Some(handler) = load_handler(handler) {
        handler(frame);
    }
}

/// Traps into the switch handler registered with [`set_switch_handler`].
///
/// The trap saves this thread's full state like an interrupt does, even with
/// IRQs masked, and the thread resumes after this call when it is next chosen.
pub fn request_switch() {
    // SAFETY: the handler restores every register before resuming this thread.
    unsafe { asm!("svc #0", options(nostack)) }
}

/// Unmasks IRQs (`DAIF.I = 0`).
pub fn enable() {
    // SAFETY: unmasking is a barrier; the handler preserves this thread's state.
    unsafe { asm!("msr daifclr, #2", options(nostack, preserves_flags)) }
}

/// Masks IRQs (`DAIF.I = 1`).
pub fn disable() {
    // SAFETY: masking interrupts has no other effect.
    unsafe { asm!("msr daifset, #2", options(nostack, preserves_flags)) }
}

/// Masks IRQs and returns the previous `DAIF` value.
///
/// Pass the result to [`restore`] to re-enable interrupts only if they were
/// enabled before the critical section.
pub fn save_and_disable() -> u64 {
    feos_sync::irq::save_and_disable()
}

/// Restores a `DAIF` value captured by [`save_and_disable`].
pub fn restore(flags: u64) {
    feos_sync::irq::restore(flags);
}

/// Returns `true` when IRQs are unmasked on this CPU.
pub fn is_enabled() -> bool {
    feos_sync::irq::is_enabled()
}

/// Waits for an interrupt or other wake-up event.
pub fn wait_for_interrupt() {
    // SAFETY: `wfi` is available at EL1 and only suspends execution.
    unsafe { asm!("wfi", options(nostack)) }
}
