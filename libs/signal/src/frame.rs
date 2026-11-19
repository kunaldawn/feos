//! Signal frames, as byte layouts (stage 1c.9; section 8.4 and 8.5 of the ABI design):
//! what the kernel writes on a user stack to enter a handler, and how it rebuilds a
//! program's registers from the context a handler's `SIGRETURN` gives back.
//!
//! Both ports are here, whatever the host, each with its registers as plain values
//! ([`x86_64::Regs`], [`aarch64::Regs`]) that the arch crates convert their trap frames to
//! and from, so the tests check both layouts and both checks anywhere.
//!
//! The builder fills a buffer it zeroes first, field by field at the offsets the ABI's
//! `ucontext` structs assert, so no padding or reserved byte carries anything of the
//! kernel's. The checker reads a copy the kernel made once, and builds the registers it
//! returns from a whitelist of the context's fields: anything else in the frame it
//! resumes from (the selectors, the privilege level, the vector) comes from the frame the
//! program is in now, which is a user frame, or is fixed.

use core::mem::offset_of;

use feos_abi::ucontext;
use feos_abi::{sig, SigInfo, SigStack};

/// What a handler is entered with, beside the registers it interrupted.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub signal: u32,
    /// The handler's address.
    pub handler: u64,
    /// What it returns to: the program's own `SIGRETURN` stub.
    pub restorer: u64,
    pub info: SigInfo,
    /// The mask to restore when the handler returns: the thread's before delivery.
    pub mask: u64,
    /// The alternate stack as it was.
    pub stack: SigStack,
    /// The address of a memory fault, or 0.
    pub fault_address: u64,
    /// The top of the alternate stack if the frame goes there; below the interrupted
    /// stack pointer otherwise.
    pub alt_top: Option<u64>,
}

/// The user window, `[base, end)`, that a context's program counter and stack pointer
/// must be in.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub base: u64,
    pub end: u64,
}

impl Window {
    fn holds_pc(&self, pc: u64) -> bool {
        pc >= self.base && pc < self.end
    }

    /// A stack pointer may be one past the top: a push writes below it.
    fn holds_sp(&self, sp: u64) -> bool {
        sp >= self.base && sp <= self.end
    }
}

/// Why a context was refused: the process then ends with `SIGSEGV`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Its `flags` aren't a format this kernel knows.
    Flags,
    /// The program counter is outside the window.
    Pc,
    /// The stack pointer is outside the window.
    Sp,
    /// x86_64: `MXCSR` has a reserved bit set, which would fault `fxrstor` in the kernel.
    Mxcsr,
}

/// What a restored mask may hold: never `SIGKILL` or `SIGSTOP`.
pub const fn restorable_mask(mask: u64) -> u64 {
    mask & !(1 << (sig::KILL - 1) | 1 << (sig::STOP - 1))
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    out[at..at + bytes.len()].copy_from_slice(bytes);
}

fn put64(out: &mut [u8], at: usize, value: u64) {
    put(out, at, &value.to_ne_bytes());
}

fn get64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_ne_bytes(word)
}

fn get32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_ne_bytes(word)
}

fn put_stack(out: &mut [u8], at: usize, stack: &SigStack) {
    put64(out, at, stack.sp);
    put(out, at + 8, &stack.flags.to_ne_bytes());
    put64(out, at + 16, stack.size);
}

pub mod x86_64 {
    use super::*;
    use feos_abi::ucontext::greg;

    type Context = ucontext::X86_64;

    /// The context's size.
    pub const CONTEXT_SIZE: usize = core::mem::size_of::<Context>();
    /// The whole frame from the handler's entry stack pointer: the restorer's address,
    /// the context and the `SigInfo`.
    pub const FRAME_SIZE: usize = 8 + CONTEXT_SIZE + SigInfo::SIZE;
    /// Below the interrupted stack pointer, the System V red zone is the interrupted
    /// code's.
    pub const RED_ZONE: u64 = 128;

    /// `RFLAGS` bits a context may set: `CF`, `PF`, `AF`, `ZF`, `SF`, `DF`, `OF`, `AC`.
    pub const RFLAGS_KEPT: u64 = 1 | 1 << 2 | 1 << 4 | 1 << 6 | 1 << 7 | 1 << 10 | 1 << 11 | 1 << 18;
    /// Set in every user `RFLAGS`: `IF`, and bit 1, which is always one.
    pub const RFLAGS_FIXED: u64 = 1 << 9 | 1 << 1;
    const DF: u64 = 1 << 10;
    const TF: u64 = 1 << 8;
    const RF: u64 = 1 << 16;

    /// Where `MXCSR` is in an `FXSAVE64` image.
    pub const FX_MXCSR: usize = 24;
    const FX_FCW: usize = 0;
    /// A handler's x87 control word and `MXCSR`: `fninit`'s and the reset value.
    pub const DEFAULT_FCW: u16 = 0x037f;
    pub const DEFAULT_MXCSR: u32 = 0x1f80;

    /// A user program's registers on x86_64, as a trap frame holds them.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Regs {
        pub r8: u64,
        pub r9: u64,
        pub r10: u64,
        pub r11: u64,
        pub r12: u64,
        pub r13: u64,
        pub r14: u64,
        pub r15: u64,
        pub rdi: u64,
        pub rsi: u64,
        pub rbp: u64,
        pub rbx: u64,
        pub rdx: u64,
        pub rax: u64,
        pub rcx: u64,
        pub rsp: u64,
        pub rip: u64,
        pub rflags: u64,
        pub cs: u64,
        pub ss: u64,
        /// The trap's vector, and its error code.
        pub vector: u64,
        pub error: u64,
        /// The `FXSAVE64` image.
        pub fx: [u8; 512],
    }

    impl Regs {
        pub const ZERO: Self = Self {
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rdi: 0,
            rsi: 0,
            rbp: 0,
            rbx: 0,
            rdx: 0,
            rax: 0,
            rcx: 0,
            rsp: 0,
            rip: 0,
            rflags: 0,
            cs: 0,
            ss: 0,
            vector: 0,
            error: 0,
            fx: [0; 512],
        };

        fn general(&self) -> [u64; 16] {
            [
                self.r8, self.r9, self.r10, self.r11, self.r12, self.r13, self.r14, self.r15, self.rdi, self.rsi, self.rbp,
                self.rbx, self.rdx, self.rax, self.rcx, self.rsp,
            ]
        }

        fn set_general(&mut self, gregs: &[u64; 16]) {
            [
                self.r8, self.r9, self.r10, self.r11, self.r12, self.r13, self.r14, self.r15, self.rdi, self.rsi, self.rbp,
                self.rbx, self.rdx, self.rax, self.rcx, self.rsp,
            ] = *gregs;
        }
    }

    const GREGS: usize = offset_of!(Context, gregs);
    const FXSAVE: usize = offset_of!(Context, fxsave);

    /// Builds the frame for `entry` interrupting `regs` into `out` (zeroed first), and the
    /// registers the handler starts with. `out` goes at the returned stack pointer,
    /// `None` if there's no room below it at all.
    pub fn build(regs: &Regs, entry: &Entry, out: &mut [u8; FRAME_SIZE]) -> Option<(u64, Regs)> {
        out.fill(0);
        let top = match entry.alt_top {
            Some(top) => top,
            None => regs.rsp.checked_sub(RED_ZONE)?,
        };
        // The context 16-byte aligned, so its `FXSAVE` image is; the return address below
        // it, as a call would leave it.
        let context = top.checked_sub((CONTEXT_SIZE + SigInfo::SIZE) as u64)? & !15;
        let sp = context.checked_sub(8)?;
        put64(out, 0, entry.restorer);
        let ctx = &mut out[8..8 + CONTEXT_SIZE];
        put64(ctx, offset_of!(Context, flags), ucontext::UC_FEOS_FXSAVE);
        put_stack(ctx, offset_of!(Context, stack), &entry.stack);
        put64(ctx, offset_of!(Context, mask), entry.mask);
        for (index, value) in regs.general().iter().enumerate() {
            put64(ctx, GREGS + index * 8, *value);
        }
        put64(ctx, GREGS + greg::RIP * 8, regs.rip);
        put64(ctx, GREGS + greg::EFL * 8, regs.rflags);
        put64(ctx, GREGS + greg::CSGSFS * 8, regs.cs & 0xffff);
        put64(ctx, GREGS + greg::ERR * 8, regs.error);
        put64(ctx, GREGS + greg::TRAPNO * 8, regs.vector);
        put64(ctx, GREGS + greg::CR2 * 8, entry.fault_address);
        put64(ctx, offset_of!(Context, fpregs), context + FXSAVE as u64);
        put(ctx, FXSAVE, &regs.fx);
        put(out, 8 + CONTEXT_SIZE, &entry.info.to_bytes());

        let mut handler = *regs;
        handler.rdi = u64::from(entry.signal);
        handler.rsi = context + CONTEXT_SIZE as u64;
        handler.rdx = context;
        handler.rax = 0;
        handler.rip = entry.handler;
        handler.rsp = sp;
        handler.rflags = (regs.rflags & !(DF | TF | RF)) | RFLAGS_FIXED;
        handler.fx = [0; 512];
        handler.fx[FX_FCW..FX_FCW + 2].copy_from_slice(&DEFAULT_FCW.to_le_bytes());
        handler.fx[FX_MXCSR..FX_MXCSR + 4].copy_from_slice(&DEFAULT_MXCSR.to_le_bytes());
        Some((sp, handler))
    }

    /// The length of `int $0xf0`, the trap a call comes in by.
    pub const SYSCALL_LENGTH: u64 = 2;

    /// Puts `regs`, a call's user frame with its result in place, back to the call
    /// itself, to be made again: the program counter on the `int $0xf0`, and the call's
    /// `number` back in `rax`, where the result went. Every argument register is as the
    /// call found it.
    pub fn rewind(regs: &mut Regs, number: u64) {
        regs.rip = regs.rip.wrapping_sub(SYSCALL_LENGTH);
        regs.rax = number;
    }

    /// The registers and mask a context gives back, checked: `current` is the frame the
    /// `SIGRETURN` came from, a user frame, whose selectors, vector and error code are
    /// kept. `mxcsr_mask` is the CPU's `MXCSR_MASK`.
    pub fn restore(current: &Regs, context: &[u8; CONTEXT_SIZE], window: Window, mxcsr_mask: u32) -> Result<(Regs, u64), Refused> {
        if get64(context, offset_of!(Context, flags)) != ucontext::UC_FEOS_FXSAVE {
            return Err(Refused::Flags);
        }
        let mut gregs = [0u64; 16];
        for (index, value) in gregs.iter_mut().enumerate() {
            *value = get64(context, GREGS + index * 8);
        }
        let rip = get64(context, GREGS + greg::RIP * 8);
        if !window.holds_pc(rip) {
            return Err(Refused::Pc);
        }
        if !window.holds_sp(gregs[greg::RSP]) {
            return Err(Refused::Sp);
        }
        let mxcsr = get32(context, FXSAVE + FX_MXCSR);
        if mxcsr & !mxcsr_mask != 0 {
            return Err(Refused::Mxcsr);
        }
        let mut regs = Regs::ZERO;
        regs.set_general(&gregs);
        regs.rip = rip;
        regs.rflags = (get64(context, GREGS + greg::EFL * 8) & RFLAGS_KEPT) | RFLAGS_FIXED;
        regs.cs = current.cs;
        regs.ss = current.ss;
        regs.vector = current.vector;
        regs.error = current.error;
        regs.fx.copy_from_slice(&context[FXSAVE..FXSAVE + 512]);
        Ok((regs, restorable_mask(get64(context, offset_of!(Context, mask)))))
    }
}

pub mod aarch64 {
    use super::*;

    type Context = ucontext::Aarch64;

    pub const CONTEXT_SIZE: usize = core::mem::size_of::<Context>();
    /// The context and the `SigInfo`, from the handler's entry stack pointer.
    pub const FRAME_SIZE: usize = CONTEXT_SIZE + SigInfo::SIZE;

    /// `NZCV`: all of `PSTATE` a context gives back.
    pub const NZCV: u64 = 0xf000_0000;
    /// `FPSR`'s architected bits: `NZCV`, `QC` and the cumulative exception flags.
    pub const FPSR_BITS: u64 = 0xf800_009f;
    /// `FPCR`'s architected bits: `AHP`, `DN`, `FZ`, `RMode`, `Stride`, `FZ16`, `Len`
    /// and the trap enables.
    pub const FPCR_BITS: u64 = 0x07ff_9f00;

    /// A user program's registers on AArch64, as a trap frame holds them.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Regs {
        pub x: [u64; 31],
        pub sp: u64,
        pub pc: u64,
        /// `SPSR_EL1` of the trap.
        pub pstate: u64,
        pub fpsr: u64,
        pub fpcr: u64,
        pub q: [u128; 32],
    }

    impl Regs {
        pub const ZERO: Self = Self { x: [0; 31], sp: 0, pc: 0, pstate: 0, fpsr: 0, fpcr: 0, q: [0; 32] };
    }

    const REGS: usize = offset_of!(Context, regs);
    const V: usize = offset_of!(Context, v);

    /// Builds the frame for `entry` interrupting `regs` into `out` (zeroed first), and the
    /// registers the handler starts with, whose `SPSR` is `user_spsr` (EL0t, and the
    /// port's masks). `out` goes at the returned stack pointer.
    pub fn build(regs: &Regs, entry: &Entry, out: &mut [u8; FRAME_SIZE], user_spsr: u64) -> Option<(u64, Regs)> {
        out.fill(0);
        let top = entry.alt_top.unwrap_or(regs.sp);
        let context = top.checked_sub(FRAME_SIZE as u64)? & !15;
        let ctx = &mut out[..CONTEXT_SIZE];
        put_stack(ctx, offset_of!(Context, stack), &entry.stack);
        put64(ctx, offset_of!(Context, mask), entry.mask);
        put64(ctx, offset_of!(Context, fault_address), entry.fault_address);
        for (index, value) in regs.x.iter().enumerate() {
            put64(ctx, REGS + index * 8, *value);
        }
        put64(ctx, offset_of!(Context, sp), regs.sp);
        put64(ctx, offset_of!(Context, pc), regs.pc);
        put64(ctx, offset_of!(Context, pstate), regs.pstate);
        put(ctx, offset_of!(Context, fpsr), &(regs.fpsr as u32).to_ne_bytes());
        put(ctx, offset_of!(Context, fpcr), &(regs.fpcr as u32).to_ne_bytes());
        for (index, value) in regs.q.iter().enumerate() {
            put(ctx, V + index * 16, &value.to_ne_bytes());
        }
        put(out, CONTEXT_SIZE, &entry.info.to_bytes());

        let mut handler = *regs;
        handler.x[0] = u64::from(entry.signal);
        handler.x[1] = context + CONTEXT_SIZE as u64;
        handler.x[2] = context;
        handler.x[30] = entry.restorer;
        handler.sp = context;
        handler.pc = entry.handler;
        handler.pstate = user_spsr;
        handler.fpsr = 0;
        handler.fpcr = 0;
        Some((context, handler))
    }

    /// The length of `svc #0`, the trap a call comes in by.
    pub const SYSCALL_LENGTH: u64 = 4;

    /// Puts `regs`, a call's user frame with its result in place, back to the call
    /// itself, to be made again: the program counter on the `svc`, and `x0`, where the
    /// result went, back to `first`, the call's first argument as it came in. The number
    /// is still in `x8`, and every other argument where it was.
    pub fn rewind(regs: &mut Regs, first: u64) {
        regs.pc = regs.pc.wrapping_sub(SYSCALL_LENGTH);
        regs.x[0] = first;
    }

    /// The registers and mask a context gives back, checked. `SPSR` is `NZCV` from the
    /// context and `user_spsr` for the rest, so the program can only come back at EL0t.
    pub fn restore(context: &[u8; CONTEXT_SIZE], window: Window, user_spsr: u64) -> Result<(Regs, u64), Refused> {
        if get64(context, offset_of!(Context, flags)) != 0 {
            return Err(Refused::Flags);
        }
        let pc = get64(context, offset_of!(Context, pc));
        let sp = get64(context, offset_of!(Context, sp));
        if !window.holds_pc(pc) {
            return Err(Refused::Pc);
        }
        if !window.holds_sp(sp) {
            return Err(Refused::Sp);
        }
        let mut regs = Regs::ZERO;
        for (index, value) in regs.x.iter_mut().enumerate() {
            *value = get64(context, REGS + index * 8);
        }
        regs.sp = sp;
        regs.pc = pc;
        regs.pstate = (get64(context, offset_of!(Context, pstate)) & NZCV) | (user_spsr & !NZCV);
        regs.fpsr = u64::from(get32(context, offset_of!(Context, fpsr))) & FPSR_BITS;
        regs.fpcr = u64::from(get32(context, offset_of!(Context, fpcr))) & FPCR_BITS;
        for (index, value) in regs.q.iter_mut().enumerate() {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&context[V + index * 16..V + index * 16 + 16]);
            *value = u128::from_ne_bytes(bytes);
        }
        Ok((regs, restorable_mask(get64(context, offset_of!(Context, mask)))))
    }
}

/// The largest frame and context either port builds, for buffers that fit both.
pub const FRAME_MAX: usize = if x86_64::FRAME_SIZE > aarch64::FRAME_SIZE { x86_64::FRAME_SIZE } else { aarch64::FRAME_SIZE };
pub const CONTEXT_MAX: usize =
    if x86_64::CONTEXT_SIZE > aarch64::CONTEXT_SIZE { x86_64::CONTEXT_SIZE } else { aarch64::CONTEXT_SIZE };

/// A port's trap frame, as a signal sees it: implemented by each arch crate over the
/// builder and checker above, so the kernel builds and checks frames without knowing
/// which port it's on.
pub trait UserFrame: Sized {
    /// This port's frame, from the handler's entry stack pointer, and context.
    const FRAME_SIZE: usize;
    const CONTEXT_SIZE: usize;

    /// The frame for `entry` interrupting this user frame, in `out[..FRAME_SIZE]`, which
    /// goes at the returned address, and the frame the handler starts from.
    fn enter_handler(&self, entry: &Entry, out: &mut [u8]) -> Option<(u64, Self)>;

    /// The frame and mask `context` (`CONTEXT_SIZE` bytes, copied once from the program)
    /// gives back, checked, from this frame, the user frame of the `SIGRETURN`.
    fn sigreturn(&self, context: &[u8], window: Window) -> Result<(Self, u64), Refused>;

    /// Puts this frame, of a call that has its result in place, back to the call, to be
    /// made again (`SA_RESTART`): the port's `rewind`, with the call's `number` and its
    /// `first` argument as they came in, which the kernel kept.
    fn rewind_syscall(&mut self, number: u64, first: u64);
}
