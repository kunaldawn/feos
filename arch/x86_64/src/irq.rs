//! Recoverable interrupt entry and the saved register frame used by the
//! scheduler.
//!
//! `interrupts.S` contains one stub per vector. Every stub pushes an error
//! code placeholder (or relies on the one the CPU pushed) and the vector
//! number, then falls into `interrupt_common`, which saves all general-purpose
//! and XMM registers and calls [`x86_interrupt_dispatch`].
//!
//! As on AArch64, the handler may replace the frame it receives. The return
//! path restores whatever frame is on the stack, so the kernel scheduler can
//! switch processes by copying a different saved frame into place.

use core::arch::{asm, global_asm};
use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::cpu;

global_asm!(include_str!("interrupts.S"), options(att_syntax));

/// Saved CPU state at the moment an interrupt was taken.
///
/// The layout is part of the contract with `interrupt_common` in
/// `interrupts.S`. The first 256 bytes hold XMM0..XMM15 (16 bytes each),
/// followed by the general-purpose registers in push order. The frame is
/// 8-byte aligned, not 16-byte aligned, because an interrupt can arrive in the
/// middle of a function prologue.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    /// `xmm0`..`xmm15`, at offsets 0..256.
    pub xmm: [u64; 32],
    /// `r15`..`rax`, at offsets 256..376.
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
    /// Interrupt vector (offset 376).
    pub vector: u64,
    /// CPU error code, or a zero placeholder (offset 384).
    pub error: u64,
    /// Interrupted instruction pointer (offset 392).
    pub rip: u64,
    /// Interrupted code segment (offset 400).
    pub cs: u64,
    /// Interrupted flags (offset 408).
    pub rflags: u64,
    /// Interrupted stack pointer (offset 416).
    pub rsp: u64,
}

// The layout is a hard contract with `interrupts.S`.
const _: () = assert!(core::mem::size_of::<TrapFrame>() == 424);
const _: () = assert!(core::mem::align_of::<TrapFrame>() == 8);

impl TrapFrame {
    /// A frame with every register cleared.
    pub const ZERO: Self = Self {
        xmm: [0; 32],
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
    };

    /// Builds the initial frame for a process that has never run.
    ///
    /// `entry` receives `arg` in the System V `rdi` register, `stack_top` is
    /// the process stack, and a return from `entry` jumps to `lr`.
    pub fn new_task(entry: usize, arg: usize, stack_top: usize, lr: usize) -> Self {
        let mut frame = Self::ZERO;
        let stack = (stack_top & !0xf).saturating_sub(8);

        frame.rdi = arg as u64;
        frame.rip = entry as u64;
        frame.cs = 0x08;
        // IF set, reserved bit 1 set.
        frame.rflags = 0x202;
        frame.rsp = stack as u64;

        // SAFETY: `stack` is inside the process's private .bss stack and is
        // the word just below the initial stack pointer.
        unsafe { core::ptr::write(stack as *mut u64, lr as u64) };
        frame
    }
}

/// Callback invoked with the frame of the interrupted process.
///
/// The handler may modify the frame; the stub restores it before returning.
pub type IrqHandler = fn(&mut TrapFrame);

static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Installs the function called for every hardware interrupt.
pub fn set_handler(handler: IrqHandler) {
    HANDLER.store(handler as usize, Ordering::Release);
}

fn handler() -> Option<IrqHandler> {
    let address = HANDLER.load(Ordering::Acquire);
    if address == 0 {
        None
    } else {
        // SAFETY: only `set_handler` writes this atomic, and a function
        // pointer is `usize`-sized on this target.
        Some(unsafe { transmute::<usize, IrqHandler>(address) })
    }
}

/// Called from `interrupt_common` in `interrupts.S`.
///
/// # Safety
/// Only called from the assembly interrupt entry stub with a valid frame.
#[no_mangle]
pub unsafe extern "C" fn x86_interrupt_dispatch(frame: *mut TrapFrame) {
    let vector = (*frame).vector;

    if vector < 32 {
        crate::exceptions::handle(&*frame);
        cpu::halt();
    }

    if let Some(handler) = handler() {
        handler(&mut *frame);
    }
}

/// Enables maskable interrupts (`RFLAGS.IF = 1`).
pub fn enable() {
    // SAFETY: changing the interrupt mask has no memory side effects.
    unsafe { asm!("sti", options(nomem, nostack)) }
}

/// Disables maskable interrupts (`RFLAGS.IF = 0`).
pub fn disable() {
    // SAFETY: changing the interrupt mask has no memory side effects.
    unsafe { asm!("cli", options(nomem, nostack)) }
}

/// Disables interrupts and returns the previous `RFLAGS` value.
pub fn save_and_disable() -> u64 {
    let flags: u64;
    // SAFETY: the stack has room for the flags word and the caller restores
    // them through `restore`.
    unsafe {
        asm!("pushfq", "pop {}", "cli", out(reg) flags)
    };
    flags
}

/// Restores an `RFLAGS` value captured by [`save_and_disable`].
pub fn restore(flags: u64) {
    // SAFETY: `flags` comes from `save_and_disable`, which read RFLAGS.
    unsafe { asm!("push {}", "popfq", in(reg) flags) };
}

/// Waits for an interrupt.
///
/// `hlt` only wakes when interrupts are enabled, so unmask them first. The
/// idle and shell wait paths call this with interrupts already enabled; the
/// explicit `sti` also makes it safe from a critical section.
pub fn wait_for_interrupt() {
    // SAFETY: `sti`/`hlt` are available at ring 0.
    unsafe { asm!("sti", "hlt", options(nomem, nostack)) }
}
