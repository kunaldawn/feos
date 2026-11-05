//! Recoverable traps and the saved register frame used by the scheduler.
//!
//! The vector table in `vectors.S` saves every general-purpose and SIMD register
//! directly into the [`TrapFrame`] of what was running on this CPU, which
//! [`set_trap_frame`] said where to find, and calls [`trap_dispatch`] on a private
//! stack to service the trap. A handler can ask for another frame to be resumed with
//! [`resume`], which also makes it the one the next trap saves into. Switching tasks
//! is a change of pointer, then, not a copy.
//!
//! Threads run on `SP_EL0`, so hardware-selected `SP_EL1` is never a stack the
//! interrupted thread owns. Once the scheduler has offered a task to other CPUs,
//! this CPU no longer touches that task's stack or frame.
//!
//! Faults and exceptions from other vector groups go through
//! [`crate::exceptions`].

use core::arch::asm;
use core::mem::{offset_of, transmute};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::exceptions;

/// Vector index of an IRQ taken from a thread (current EL with `SP_EL0`).
const VECTOR_THREAD_IRQ: u64 = 1;
/// Vector index of a synchronous exception taken from a thread.
const VECTOR_THREAD_SYNC: u64 = 0;
/// Vector index of a synchronous exception from a user program.
const VECTOR_USER_SYNC: u64 = 8;
/// Vector index of an IRQ taken from a user program.
const VECTOR_USER_IRQ: u64 = 9;
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

/// `SPSR_EL1` for a user program: EL0t, with the same masks. It can't change them:
/// `SCTLR_EL1.UMA` is clear, so `msr daif` from EL0 traps.
const SPSR_USER: u64 = (1 << 9) | (1 << 8) | (1 << 6);

/// `PSTATE.PAN`, in the form `SPSR_EL1` has it, when this machine has PAN on.
const SPSR_PAN: u64 = 1 << 22;
static THREAD_PAN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// New threads start with PAN set, as the rest of the kernel runs.
pub(crate) fn set_thread_pan() {
    THREAD_PAN.store(SPSR_PAN, Ordering::Relaxed);
}

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
        frame.spsr = SPSR_THREAD | THREAD_PAN.load(Ordering::Relaxed);
        frame
    }

    /// Builds the initial frame of a user program.
    pub fn new_user(entry: usize, stack_top: usize, arg0: usize, arg1: usize) -> Self {
        let mut frame = Self::ZERO;
        frame.x[0] = arg0 as u64;
        frame.x[1] = arg1 as u64;
        frame.sp = stack_top as u64;
        frame.elr = entry as u64;
        frame.spsr = SPSR_USER;
        frame
    }

    /// Whether this is a user program's state: a trap from EL0t.
    pub fn is_user(&self) -> bool {
        self.spsr & 0xf == 0
    }

    /// Stack pointer of the interrupted context.
    pub fn stack_pointer(&self) -> usize {
        self.sp as usize
    }

    /// Whether the trapped code could take interrupts. If it could, it wasn't an
    /// interrupt handler, and held no lock that masks them.
    pub fn interrupts_were_enabled(&self) -> bool {
        self.spsr & (1 << 7) == 0
    }

    /// The interrupt that caused this trap, when the CPU says. It doesn't here:
    /// only the interrupt controller knows.
    pub fn interrupt_id(&self) -> Option<u32> {
        None
    }
}

/// Callback invoked with the frame of the interrupted process.
///
/// The stub restores that frame when this returns, unless the handler asks for
/// another with [`resume`].
pub type IrqHandler = fn(&mut TrapFrame);

static HANDLER: AtomicUsize = AtomicUsize::new(0);
static SWITCH_HANDLER: AtomicUsize = AtomicUsize::new(0);
static SYSCALL_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Installs the function called for every IRQ from EL1.
pub fn set_handler(handler: IrqHandler) {
    HANDLER.store(handler as usize, Ordering::Release);
}

/// Installs the function called when a thread uses [`request_switch`].
pub fn set_switch_handler(handler: IrqHandler) {
    SWITCH_HANDLER.store(handler as usize, Ordering::Release);
}

/// Installs the function called when a user program makes a system call.
pub fn set_syscall_handler(handler: IrqHandler) {
    SYSCALL_HANDLER.store(handler as usize, Ordering::Release);
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

/// A frame a handler on each CPU asked to resume, or null.
static RESUME: [AtomicPtr<TrapFrame>; crate::smp::MAX_CPUS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; crate::smp::MAX_CPUS];

/// Has traps on this CPU save their state in `frame` from now on.
///
/// Call with interrupts masked, for the frame of what's about to run, on a CPU not
/// yet switching through [`resume`].
pub fn set_trap_frame(frame: *mut TrapFrame) {
    let top = frame as usize + core::mem::size_of::<TrapFrame>();
    // SAFETY: with interrupts masked and on SP_EL0, nothing is using SP_EL1.
    unsafe {
        asm!(
            "msr spsel, #1",
            "mov sp, {top}",
            "msr spsel, #0",
            top = in(reg) top,
            options(nostack, preserves_flags)
        );
    }
}

/// From a handler: returns to `frame` instead of the frame the trap saved, and has
/// the next trap on this CPU save there.
///
/// `frame` must be a complete frame, 16-byte aligned, that stays put.
pub fn resume(frame: *mut TrapFrame) {
    RESUME[crate::cpu::index().min(crate::smp::MAX_CPUS - 1)].store(frame, Ordering::Relaxed);
}

/// Called from `trap_common` in `vectors.S`. Returns the frame to resume.
///
/// Returns for an IRQ or a software switch from a thread, or when the fault handler
/// has ended the faulting thread. Anything else stops the CPU.
///
/// # Safety
/// Only called from the assembly trap stub with a valid frame pointer.
#[no_mangle]
unsafe extern "C" fn trap_dispatch(index: u64, saved: *mut TrapFrame) -> *mut TrapFrame {
    // SAFETY: the stub passes the frame it just saved.
    let frame = unsafe { &mut *saved };
    let esr: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, esr_el1", out(reg) esr, options(nomem, nostack)) };

    let handler = match index {
        VECTOR_THREAD_IRQ | VECTOR_USER_IRQ => &HANDLER,
        VECTOR_THREAD_SYNC if (esr >> 26) & 0x3f == ESR_CLASS_SVC => &SWITCH_HANDLER,
        VECTOR_USER_SYNC if (esr >> 26) & 0x3f == ESR_CLASS_SVC => &SYSCALL_HANDLER,
        _ => {
            // A handler that returns has dealt with it, and asked for another frame.
            exceptions::fault(index, esr, frame);
            return take_resume(saved);
        }
    };
    if let Some(handler) = load_handler(handler) {
        handler(frame);
    }
    take_resume(saved)
}

fn take_resume(saved: *mut TrapFrame) -> *mut TrapFrame {
    let slot = &RESUME[crate::cpu::index().min(crate::smp::MAX_CPUS - 1)];
    let next = slot.swap(core::ptr::null_mut(), Ordering::Relaxed);
    if next.is_null() {
        saved
    } else {
        next
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
