//! Recoverable interrupt entry and the saved register frame used by the
//! scheduler.
//!
//! `interrupts.S` contains one stub per vector. Every stub pushes an error
//! code placeholder (or relies on the one the CPU pushed) and the vector
//! number, then falls into `interrupt_common`, which saves all general-purpose
//! and floating-point registers and calls [`x86_interrupt_dispatch`].
//!
//! The kernel is built with SSE2, so any Rust may use XMM registers, handlers
//! included. Every trap therefore saves the whole FP state before calling Rust and
//! restores it on the way out, which also makes it per-task: it's part of the
//! frame a switch replaces.
//!
//! An interrupt saves the frame directly into the [`TrapFrame`] of what was running
//! on this CPU, which [`set_trap_frame`] said where to find. The handler then runs
//! on a per-CPU stack: once the scheduler has offered a task to other CPUs, this
//! CPU no longer touches that task's stack or frame.
//!
//! A handler can ask for another frame to be resumed with [`resume`], which also
//! makes it the one the next interrupt saves into. Switching tasks is a change of
//! pointer, then, not a copy.

use core::arch::{asm, global_asm};
use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{cpu, gdt};


/// Vector of the software switch used by [`request_switch`].
pub const SWITCH_VECTOR: u64 = 48;

/// Vector of a user program's system call: `int $0xf0`, the one gate ring 3 may use.
/// It's above the devices' range, and has to stay equal to the runtime's trap.
pub const SYSCALL_VECTOR: u64 = 0xf0;

global_asm!(include_str!("interrupts.S"), options(att_syntax));

/// Saved CPU state at the moment an interrupt was taken.
///
/// Since the frame is where the CPU itself writes, it is exactly as long as what's
/// pushed, ending on a 16-byte boundary.
///
/// The layout is part of the contract with `interrupt_common` in
/// `interrupts.S`. The first 512 bytes are an `FXSAVE64` image, followed by the
/// general-purpose registers in push order. The whole frame is 16-byte aligned,
/// which `FXSAVE` requires.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    /// x87, MMX and XMM registers and MXCSR, as `FXSAVE64` writes them.
    pub fx: [u8; 512],
    /// `r15`..`rax`, at offsets 512..632.
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    /// Interrupt vector (offset 632).
    pub vector: u64,
    /// CPU error code, or a zero placeholder (offset 640).
    pub error: u64,
    /// Interrupted instruction pointer (offset 648).
    pub rip: u64,
    /// Interrupted code segment (offset 656).
    pub cs: u64,
    /// Interrupted flags (offset 664).
    pub rflags: u64,
    /// Interrupted stack pointer (offset 672).
    pub rsp: u64,
    /// Interrupted stack segment (offset 680).
    pub ss: u64,
}

// The layout is a hard contract with `interrupts.S`. Long mode always pushes
// SS:RSP with the interrupt frame, even for a same-privilege interrupt.
const _: () = assert!(core::mem::size_of::<TrapFrame>() == 688);
const _: () = assert!(core::mem::align_of::<TrapFrame>() == 16);
const _: () = assert!(core::mem::offset_of!(TrapFrame, fx) == 0);
const _: () = assert!(core::mem::offset_of!(TrapFrame, r15) == 512);
const _: () = assert!(core::mem::offset_of!(TrapFrame, rax) == 624);
const _: () = assert!(core::mem::offset_of!(TrapFrame, vector) == 632);
const _: () = assert!(core::mem::offset_of!(TrapFrame, error) == 640);
const _: () = assert!(core::mem::offset_of!(TrapFrame, rip) == 648);
const _: () = assert!(core::mem::offset_of!(TrapFrame, cs) == 656);
const _: () = assert!(core::mem::offset_of!(TrapFrame, rflags) == 664);
const _: () = assert!(core::mem::offset_of!(TrapFrame, rsp) == 672);
const _: () = assert!(core::mem::offset_of!(TrapFrame, ss) == 680);

/// FXSAVE image offsets of the values a task must start with: `fninit`'s control
/// word and MXCSR's reset value. A zero MXCSR would unmask every SSE exception.
const FX_FCW: usize = 0;
const FX_MXCSR: usize = 24;
const DEFAULT_FCW: u16 = 0x037f;
const DEFAULT_MXCSR: u32 = 0x1f80;

impl TrapFrame {
    /// A frame with every register cleared.
    pub const ZERO: Self = Self {
        fx: [0; 512],
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rbp: 0,
        rdi: 0,
        rsi: 0,
        rdx: 0,
        rcx: 0,
        rbx: 0,
        rax: 0,
        vector: 0,
        error: 0,
        rip: 0,
        cs: 0,
        rflags: 0,
        rsp: 0,
        ss: 0,
    };

    /// Builds the initial frame for a process that has never run.
    ///
    /// `entry` receives `arg` in the System V `rdi` register, `stack_top` is
    /// the process stack, and a return from `entry` jumps to `lr`.
    pub fn new_task(entry: usize, arg: usize, stack_top: usize, lr: usize) -> Self {
        let mut frame = Self::ZERO;
        let stack = (stack_top & !0xf).saturating_sub(8);

        frame.fx[FX_FCW..FX_FCW + 2].copy_from_slice(&DEFAULT_FCW.to_le_bytes());
        frame.fx[FX_MXCSR..FX_MXCSR + 4].copy_from_slice(&DEFAULT_MXCSR.to_le_bytes());
        frame.rdi = arg as u64;
        frame.rip = entry as u64;
        frame.cs = 0x08;
        // IF set, reserved bit 1 set.
        frame.rflags = 0x202;
        frame.rsp = stack as u64;
        frame.ss = 0x10;

        // SAFETY: `stack` is inside the process's private .bss stack and is
        // the word just below the initial stack pointer.
        unsafe { core::ptr::write(stack as *mut u64, lr as u64) };
        frame
    }

    /// Builds the initial frame of a user program.
    pub fn new_user(entry: usize, stack_top: usize, arg0: usize, arg1: usize) -> Self {
        let mut frame = Self::ZERO;
        frame.fx[FX_FCW..FX_FCW + 2].copy_from_slice(&DEFAULT_FCW.to_le_bytes());
        frame.fx[FX_MXCSR..FX_MXCSR + 4].copy_from_slice(&DEFAULT_MXCSR.to_le_bytes());
        frame.rdi = arg0 as u64;
        frame.rsi = arg1 as u64;
        frame.rip = entry as u64;
        frame.cs = gdt::USER_CODE;
        // IF set, reserved bit 1 set, IOPL zero.
        frame.rflags = 0x202;
        frame.rsp = stack_top as u64;
        frame.ss = gdt::USER_DATA;
        frame
    }

    /// Whether this is a user program's state: a trap from ring 3.
    pub fn is_user(&self) -> bool {
        self.cs & 3 == 3
    }

    /// Stack pointer of the interrupted context.
    pub fn stack_pointer(&self) -> usize {
        self.rsp as usize
    }

    /// Whether the trapped code could take interrupts. If it could, it wasn't an
    /// interrupt handler, and held no lock that masks them.
    pub fn interrupts_were_enabled(&self) -> bool {
        self.rflags & (1 << 9) != 0
    }

    /// The interrupt that caused this trap, which x86 hands over at entry, so
    /// nothing has to ask the interrupt controller.
    pub fn interrupt_id(&self) -> Option<u32> {
        Some(self.vector as u32)
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

/// Installs the function called for every hardware interrupt.
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
        // SAFETY: only the setters above write these atomics, and a function
        // pointer is `usize`-sized on this target.
        Some(unsafe { transmute::<usize, IrqHandler>(address) })
    }
}

/// Called from `interrupt_common` in `interrupts.S`. Returns the frame to resume.
///
/// # Safety
/// Only called from the assembly interrupt entry stub with a valid frame.
#[no_mangle]
pub unsafe extern "C" fn x86_interrupt_dispatch(frame: *mut TrapFrame) -> *mut TrapFrame {
    let vector = (*frame).vector;

    if vector < 32 {
        // A handler that returns has dealt with it, and asked for another frame.
        crate::exceptions::handle(&mut *frame);
    } else {
        let slot = if vector == SWITCH_VECTOR {
            &SWITCH_HANDLER
        } else if vector == SYSCALL_VECTOR {
            &SYSCALL_HANDLER
        } else {
            &HANDLER
        };
        if let Some(handler) = load_handler(slot) {
            handler(&mut *frame);
        }
    }

    match cpu::area().take_resume() {
        0 => frame,
        next => {
            gdt::set_trap_frame_top(next + core::mem::size_of::<TrapFrame>() as u64);
            next as *mut TrapFrame
        }
    }
}

/// Has traps on this CPU save their state in `frame` from now on.
///
/// Call with interrupts masked, for the frame of what's about to run, on a CPU not
/// yet switching through [`resume`].
pub fn set_trap_frame(frame: *mut TrapFrame) {
    gdt::set_trap_frame_top(frame as u64 + core::mem::size_of::<TrapFrame>() as u64);
}

/// From a handler: returns to `frame` instead of the frame the trap saved, and has
/// the next trap on this CPU save there.
///
/// `frame` must be a complete frame, 16-byte aligned, that stays put.
pub fn resume(frame: *mut TrapFrame) {
    cpu::area().set_resume(frame as u64);
}

/// Traps into the switch handler registered with [`set_switch_handler`].
///
/// The trap saves this thread's full state like an interrupt does, even with
/// interrupts masked, and the thread resumes after this call when it is next
/// chosen.
pub fn request_switch() {
    // SAFETY: the handler restores every register before resuming this thread,
    // and the interrupt gate moves to the per-CPU stack before pushing anything.
    unsafe { asm!("int {vector}", vector = const SWITCH_VECTOR, options(nostack)) }
}

/// Enables maskable interrupts (`RFLAGS.IF = 1`).
pub fn enable() {
    // SAFETY: the handler preserves this thread's state.
    unsafe { asm!("sti", options(nostack)) }
}

/// Disables maskable interrupts (`RFLAGS.IF = 0`).
pub fn disable() {
    // SAFETY: masking interrupts has no other effect.
    unsafe { asm!("cli", options(nostack)) }
}

/// Disables interrupts and returns the previous `RFLAGS` value.
pub fn save_and_disable() -> u64 {
    feos_sync::irq::save_and_disable()
}

/// Restores an `RFLAGS` value captured by [`save_and_disable`].
pub fn restore(flags: u64) {
    feos_sync::irq::restore(flags);
}

/// Returns `true` when interrupts are unmasked on this CPU.
pub fn is_enabled() -> bool {
    feos_sync::irq::is_enabled()
}

/// Waits for an interrupt.
///
/// `hlt` only wakes when interrupts are enabled, so unmask them first. The
/// idle and shell wait paths call this with interrupts already enabled; the
/// explicit `sti` also makes it safe from a critical section.
pub fn wait_for_interrupt() {
    // SAFETY: `sti`/`hlt` are available at ring 0.
    unsafe { asm!("sti", "hlt", options(nostack)) }
}
