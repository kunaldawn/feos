//! The frame builder and checker on both ports' layouts, and the claim of a process
//! signal, whatever the host.

use core::mem::offset_of;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use feos_abi::ucontext::{self, greg};
use feos_abi::{sa, si, sig, SigInfo, SigStack};

use crate::frame::{aarch64, x86_64, Entry, Refused, Window};
use crate::{bit, claim};

const WINDOW: Window = Window { base: 0x10_0000_0000, end: 0x80_0000_0000 };
const STACK: u64 = 0x7f_0000_8000;
/// AArch64's `SPSR_EL1` for a user frame, as the port has it: EL0t with D, A and F set.
const USER_SPSR: u64 = 1 << 9 | 1 << 8 | 1 << 6;
/// What an x86_64 CPU without a wider mask reports: everything but `DAZ`.
const MXCSR_MASK: u32 = 0xffbf;

fn entry() -> Entry {
    Entry {
        signal: sig::USR1,
        handler: 0x10_0000_4000,
        restorer: 0x10_0000_5000,
        info: SigInfo::sender(sig::USR1, si::USER, 42, 1000),
        mask: bit(sig::USR2),
        stack: SigStack { sp: 0, flags: feos_abi::ss::DISABLE, reserved: 0, size: 0 },
        fault_address: 0x1234,
        alt_top: None,
    }
}

fn x86_regs() -> x86_64::Regs {
    let mut regs = x86_64::Regs::ZERO;
    let values = [8u64, 9, 10, 11, 12, 13, 14, 15, 0xd1, 0x51, 0xb9, 0xbb, 0xdd, 0xaa, 0xcc];
    [regs.r8, regs.r9, regs.r10, regs.r11, regs.r12, regs.r13, regs.r14, regs.r15, regs.rdi, regs.rsi, regs.rbp, regs.rbx, regs.rdx, regs.rax, regs.rcx] =
        values;
    regs.rsp = STACK;
    regs.rip = 0x10_0000_1000;
    regs.rflags = 0x202 | 1 << 10 | 1;
    regs.cs = 0x23;
    regs.ss = 0x1b;
    regs.vector = 14;
    regs.error = 6;
    for (index, byte) in regs.fx.iter_mut().enumerate() {
        *byte = index as u8;
    }
    regs.fx[24..28].copy_from_slice(&0x1f80u32.to_le_bytes());
    regs
}

fn arm_regs() -> aarch64::Regs {
    let mut regs = aarch64::Regs::ZERO;
    for (index, value) in regs.x.iter_mut().enumerate() {
        *value = 0x100 + index as u64;
    }
    for (index, value) in regs.q.iter_mut().enumerate() {
        *value = (index as u128) << 64 | 0xabc;
    }
    regs.sp = STACK;
    regs.pc = 0x10_0000_1000;
    regs.pstate = 0x6000_0000 | USER_SPSR;
    regs.fpsr = 0x10;
    regs.fpcr = 0x40_0000;
    regs
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn x86_context(frame: &[u8; x86_64::FRAME_SIZE]) -> [u8; x86_64::CONTEXT_SIZE] {
    frame[8..8 + x86_64::CONTEXT_SIZE].try_into().unwrap()
}

fn arm_context(frame: &[u8; aarch64::FRAME_SIZE]) -> [u8; aarch64::CONTEXT_SIZE] {
    frame[..aarch64::CONTEXT_SIZE].try_into().unwrap()
}

#[test]
fn frame_layout_x86_64() {
    let regs = x86_regs();
    let mut frame = [0u8; x86_64::FRAME_SIZE];
    let (sp, handler) = x86_64::build(&regs, &entry(), &mut frame).unwrap();
    // `[rsp]` is the restorer, `rsp + 8` is 16-byte aligned and below the red zone.
    assert_eq!(x86_64::FRAME_SIZE, 8 + 816 + 128);
    assert_eq!((sp + 8) % 16, 0);
    assert!(sp + x86_64::FRAME_SIZE as u64 <= STACK - 128);
    assert_eq!(word(&frame, 0), entry().restorer);
    let ctx = &frame[8..];
    let context = sp + 8;
    assert_eq!(word(ctx, 0), ucontext::UC_FEOS_FXSAVE);
    assert_eq!(word(ctx, 8), 0);
    assert_eq!(i32::from_ne_bytes(ctx[24..28].try_into().unwrap()), feos_abi::ss::DISABLE);
    assert_eq!(word(ctx, 40), bit(sig::USR2));
    let gregs = |index: usize| word(ctx, 48 + index * 8);
    assert_eq!((gregs(greg::R8), gregs(greg::R15), gregs(greg::RDI), gregs(greg::RSI)), (8, 15, 0xd1, 0x51));
    assert_eq!((gregs(greg::RBP), gregs(greg::RBX), gregs(greg::RDX), gregs(greg::RAX), gregs(greg::RCX)), (0xb9, 0xbb, 0xdd, 0xaa, 0xcc));
    assert_eq!((gregs(greg::RSP), gregs(greg::RIP), gregs(greg::EFL)), (STACK, regs.rip, regs.rflags));
    assert_eq!((gregs(greg::CSGSFS), gregs(greg::ERR), gregs(greg::TRAPNO), gregs(greg::OLDMASK), gregs(greg::CR2)), (0x23, 6, 14, 0, 0x1234));
    assert_eq!(word(ctx, 232), context + 304, "fpregs points at the FXSAVE image");
    assert_eq!((context + 304) % 16, 0);
    assert_eq!(&ctx[304..816], &regs.fx[..]);
    let info = SigInfo::from_bytes(ctx[816..944].try_into().unwrap());
    assert_eq!((info.signo, info.code, info.pid(), info.uid()), (sig::USR1 as i32, si::USER, 42, 1000));
    // The handler's registers.
    assert_eq!((handler.rdi, handler.rsi, handler.rdx, handler.rax), (u64::from(sig::USR1), context + 816, context, 0));
    assert_eq!((handler.rip, handler.rsp), (entry().handler, sp));
    assert_eq!(handler.rflags & (1 << 10), 0, "direction flag clear");
    assert_ne!(handler.rflags & (1 << 9), 0);
    assert_eq!(u32::from_le_bytes(handler.fx[24..28].try_into().unwrap()), 0x1f80);
    assert_eq!(u16::from_le_bytes(handler.fx[0..2].try_into().unwrap()), 0x037f);
    assert_eq!((handler.cs, handler.ss, handler.rbx), (regs.cs, regs.ss, regs.rbx));
    // On an alternate stack, from its top.
    let mut on_alt = entry();
    on_alt.alt_top = Some(0x20_0000_2000);
    let (sp, _) = x86_64::build(&regs, &on_alt, &mut frame).unwrap();
    assert!(sp < 0x20_0000_2000 && sp + x86_64::FRAME_SIZE as u64 <= 0x20_0000_2000 && (sp + 8) % 16 == 0);
    // No room at all below the stack pointer.
    let mut low = regs;
    low.rsp = 64;
    assert!(x86_64::build(&low, &entry(), &mut frame).is_none());
}

#[test]
fn frame_layout_aarch64() {
    let regs = arm_regs();
    let mut frame = [0u8; aarch64::FRAME_SIZE];
    let (sp, handler) = aarch64::build(&regs, &entry(), &mut frame, USER_SPSR).unwrap();
    assert_eq!(aarch64::FRAME_SIZE, 880 + 128);
    assert_eq!(sp % 16, 0);
    assert!(sp + aarch64::FRAME_SIZE as u64 <= STACK);
    assert_eq!((word(&frame, 0), word(&frame, 8)), (0, 0));
    assert_eq!(word(&frame, 40), bit(sig::USR2));
    assert_eq!(word(&frame, 48), 0x1234);
    assert_eq!((word(&frame, 56), word(&frame, 56 + 30 * 8)), (0x100, 0x100 + 30));
    assert_eq!((word(&frame, 304), word(&frame, 312), word(&frame, 320)), (STACK, regs.pc, regs.pstate));
    assert_eq!(u32::from_ne_bytes(frame[328..332].try_into().unwrap()), 0x10);
    assert_eq!(u32::from_ne_bytes(frame[332..336].try_into().unwrap()), 0x40_0000);
    assert_eq!(u128::from_ne_bytes(frame[336 + 31 * 16..336 + 32 * 16].try_into().unwrap()), regs.q[31]);
    assert_eq!(offset_of!(ucontext::Aarch64, reserved), 848);
    let info = SigInfo::from_bytes(frame[880..1008].try_into().unwrap());
    assert_eq!(info.signo, sig::USR1 as i32);
    assert_eq!((handler.x[0], handler.x[1], handler.x[2], handler.x[30]), (u64::from(sig::USR1), sp + 880, sp, entry().restorer));
    assert_eq!((handler.sp, handler.pc, handler.pstate, handler.fpsr, handler.fpcr), (sp, entry().handler, USER_SPSR, 0, 0));
    assert_eq!((handler.x[3], handler.q[5]), (regs.x[3], regs.q[5]));
}

#[test]
fn frame_padding_zeroed() {
    // Whatever was in the buffer, only the fields are written: padding and reserved bytes
    // are zero.
    let mut frame = [0xaau8; x86_64::FRAME_SIZE];
    x86_64::build(&x86_regs(), &entry(), &mut frame).unwrap();
    let ctx = &frame[8..];
    assert!(ctx[28..32].iter().all(|&byte| byte == 0), "SigStack padding");
    assert!(ctx[240..304].iter().all(|&byte| byte == 0), "reserved");
    assert_eq!(word(ctx, 48 + greg::OLDMASK * 8), 0);
    assert!(ctx[816 + 48..].iter().all(|&byte| byte == 0), "the rest of the SigInfo");
    assert!(ctx[816 + 12..816 + 16].iter().all(|&byte| byte == 0));
    let mut frame = [0xaau8; aarch64::FRAME_SIZE];
    aarch64::build(&arm_regs(), &entry(), &mut frame, USER_SPSR).unwrap();
    assert!(frame[28..32].iter().all(|&byte| byte == 0));
    assert!(frame[848..880].iter().all(|&byte| byte == 0), "reserved");
    assert!(frame[880 + 24..].iter().all(|&byte| byte == 0));
}

#[test]
fn sigreturn_roundtrips_both() {
    let regs = x86_regs();
    let mut frame = [0u8; x86_64::FRAME_SIZE];
    let (_, handler) = x86_64::build(&regs, &entry(), &mut frame).unwrap();
    let (back, mask) = x86_64::restore(&handler, &x86_context(&frame), WINDOW, MXCSR_MASK).unwrap();
    assert_eq!(back, regs);
    assert_eq!(mask, bit(sig::USR2));
    let regs = arm_regs();
    let mut frame = [0u8; aarch64::FRAME_SIZE];
    aarch64::build(&regs, &entry(), &mut frame, USER_SPSR).unwrap();
    let (back, mask) = aarch64::restore(&arm_context(&frame), WINDOW, USER_SPSR).unwrap();
    assert_eq!(back, regs);
    assert_eq!(mask, bit(sig::USR2));
}

/// A context the program rewrote, for the checks.
fn x86_rewritten(change: impl FnOnce(&mut [u8; x86_64::CONTEXT_SIZE])) -> Result<(x86_64::Regs, u64), Refused> {
    let mut frame = [0u8; x86_64::FRAME_SIZE];
    let (_, handler) = x86_64::build(&x86_regs(), &entry(), &mut frame).unwrap();
    let mut context = x86_context(&frame);
    change(&mut context);
    x86_64::restore(&handler, &context, WINDOW, MXCSR_MASK)
}

fn set(context: &mut [u8], at: usize, value: u64) {
    context[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

#[test]
fn sigreturn_never_copies_cs_ss() {
    let mut frame = [0u8; x86_64::FRAME_SIZE];
    let (_, handler) = x86_64::build(&x86_regs(), &entry(), &mut frame).unwrap();
    let mut context = x86_context(&frame);
    // A ring 0 code selector in the context: the kernel's, 0x08.
    set(&mut context, 48 + greg::CSGSFS * 8, 0x08);
    set(&mut context, 48 + greg::TRAPNO * 8, 0x30);
    set(&mut context, 48 + greg::ERR * 8, 0xdead);
    let (regs, _) = x86_64::restore(&handler, &context, WINDOW, MXCSR_MASK).unwrap();
    assert_eq!((regs.cs, regs.ss), (0x23, 0x1b), "the user selectors of the frame it came from");
    assert_eq!((regs.vector, regs.error), (handler.vector, handler.error));
}

#[test]
fn sigreturn_sanitises_rflags() {
    // IOPL 3, TF, NT, RF, VM, VIF, VIP, ID and IF clear: none survives, IF is set.
    let (regs, _) = x86_rewritten(|context| set(context, 48 + greg::EFL * 8, u64::MAX & !(1 << 9))).unwrap();
    assert_eq!(regs.rflags, x86_64::RFLAGS_KEPT | x86_64::RFLAGS_FIXED);
    assert_eq!(regs.rflags & (3 << 12), 0, "IOPL 0");
    assert_eq!(regs.rflags & (1 << 8 | 1 << 14 | 1 << 16 | 1 << 17 | 1 << 19 | 1 << 20 | 1 << 21), 0);
    let (regs, _) = x86_rewritten(|context| set(context, 48 + greg::EFL * 8, 0)).unwrap();
    assert_eq!(regs.rflags, 0x202);
}

#[test]
fn sigreturn_rejects_noncanonical_rip() {
    for rip in [0x8000_0000_0000_0000u64, 0x0000_8000_0000_0000, 0xffff_8000_0000_1000, 0x20_0000, WINDOW.end] {
        assert_eq!(x86_rewritten(|context| set(context, 48 + greg::RIP * 8, rip)).unwrap_err(), Refused::Pc, "rip {rip:#x}");
    }
    for rsp in [0x8000_0000_0000_0000u64, 0xffff_ffff_ffff_fff0, 0] {
        assert_eq!(x86_rewritten(|context| set(context, 48 + greg::RSP * 8, rsp)).unwrap_err(), Refused::Sp, "rsp {rsp:#x}");
    }
    // The top of the window is a stack pointer, if not a program counter.
    assert!(x86_rewritten(|context| set(context, 48 + greg::RSP * 8, WINDOW.end)).is_ok());
}

#[test]
fn sigreturn_rejects_bad_mxcsr() {
    let fx = 304 + x86_64::FX_MXCSR;
    for mxcsr in [0x1_0000u32, 0x8000_1f80, 0x40] {
        let refused = x86_rewritten(|context| context[fx..fx + 4].copy_from_slice(&mxcsr.to_ne_bytes()));
        assert_eq!(refused.unwrap_err(), Refused::Mxcsr, "mxcsr {mxcsr:#x}");
    }
    // Every bit the mask allows is fine: exceptions unmasked, flush to zero.
    let (regs, _) = x86_rewritten(|context| context[fx..fx + 4].copy_from_slice(&0xffbfu32.to_ne_bytes())).unwrap();
    assert_eq!(u32::from_ne_bytes(regs.fx[24..28].try_into().unwrap()), 0xffbf);
    // A CPU with DAZ takes it too.
    let mut frame = [0u8; x86_64::FRAME_SIZE];
    let (_, handler) = x86_64::build(&x86_regs(), &entry(), &mut frame).unwrap();
    let mut context = x86_context(&frame);
    context[fx..fx + 4].copy_from_slice(&0x40u32.to_ne_bytes());
    assert!(x86_64::restore(&handler, &context, WINDOW, 0xffff).is_ok());
}

#[test]
fn sigreturn_builds_spsr_from_nzcv() {
    let mut frame = [0u8; aarch64::FRAME_SIZE];
    aarch64::build(&arm_regs(), &entry(), &mut frame, USER_SPSR).unwrap();
    let mut context = arm_context(&frame);
    // EL1h, every mask, PAN, UAO, SS, IL and BTYPE: only NZCV survives.
    set(&mut context, 320, u64::MAX);
    let (regs, _) = aarch64::restore(&context, WINDOW, USER_SPSR).unwrap();
    assert_eq!(regs.pstate, 0xf000_0000 | USER_SPSR);
    assert_eq!(regs.pstate & 0xf, 0, "EL0t");
    assert_eq!(regs.pstate & 1 << 7, 0, "IRQs unmasked");
    set(&mut context, 320, 0x5);
    assert_eq!(aarch64::restore(&context, WINDOW, USER_SPSR).unwrap().0.pstate, USER_SPSR);
    // FP control and status: architected bits only.
    context[328..332].copy_from_slice(&u32::MAX.to_ne_bytes());
    context[332..336].copy_from_slice(&u32::MAX.to_ne_bytes());
    let (regs, _) = aarch64::restore(&context, WINDOW, USER_SPSR).unwrap();
    assert_eq!((regs.fpsr, regs.fpcr), (aarch64::FPSR_BITS, aarch64::FPCR_BITS));
    // A kernel program counter or stack pointer.
    set(&mut context, 312, 0xffff_0000_4020_0000);
    assert_eq!(aarch64::restore(&context, WINDOW, USER_SPSR).unwrap_err(), Refused::Pc);
    set(&mut context, 312, 0x10_0000_1000);
    set(&mut context, 304, 0x4020_0000);
    assert_eq!(aarch64::restore(&context, WINDOW, USER_SPSR).unwrap_err(), Refused::Sp);
}

#[test]
fn sigreturn_rejects_unknown_flags() {
    for flags in [0u64, 2, 3, 1 << 63] {
        assert_eq!(x86_rewritten(|context| set(context, 0, flags)).unwrap_err(), Refused::Flags, "flags {flags}");
    }
    let mut frame = [0u8; aarch64::FRAME_SIZE];
    aarch64::build(&arm_regs(), &entry(), &mut frame, USER_SPSR).unwrap();
    for flags in [1u64, 2, 1 << 40] {
        let mut context = arm_context(&frame);
        set(&mut context, 0, flags);
        assert_eq!(aarch64::restore(&context, WINDOW, USER_SPSR).unwrap_err(), Refused::Flags);
    }
}

#[test]
fn sigreturn_mask_drops_kill_and_stop() {
    let (_, mask) = x86_rewritten(|context| set(context, 40, u64::MAX)).unwrap();
    assert_eq!(mask, u64::MAX & !bit(sig::KILL) & !bit(sig::STOP));
    assert_eq!(sa::RESTORER, 0x0400_0000);
}

#[test]
fn process_signal_claimed_once() {
    // Eight threads race to claim a process's signals, each with a mask of its own that
    // changes as it goes; every signal raised is taken by exactly one of them.
    const ROUNDS: usize = 20_000;
    let pending = Arc::new(AtomicU64::new(0));
    let taken: Arc<Vec<AtomicUsize>> = Arc::new((0..64).map(|_| AtomicUsize::new(0)).collect());
    let raised = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let claimers: Vec<_> = (0..8u64)
        .map(|me| {
            let (pending, taken, done) = (pending.clone(), taken.clone(), done.clone());
            std::thread::spawn(move || {
                let mut round = 0u64;
                loop {
                    let finished = done.load(Ordering::Acquire);
                    // Blocks a different few signals each time round, never all of them.
                    let mask = (0x0101_0101_0101_0101u64 << ((me + round) % 8)) & !1;
                    while let Some(signal) = claim(&pending, mask) {
                        assert_eq!(bit(signal) & mask, 0, "a blocked signal was claimed");
                        taken[signal as usize - 1].fetch_add(1, Ordering::AcqRel);
                    }
                    if finished {
                        while let Some(signal) = claim(&pending, 0) {
                            taken[signal as usize - 1].fetch_add(1, Ordering::AcqRel);
                        }
                        return;
                    }
                    round += 1;
                }
            })
        })
        .collect();
    let mut raised_each = [0usize; 64];
    for round in 0..ROUNDS {
        let signal = (round * 7 % 64) as u32 + 1;
        // Raised only when it isn't pending: standard signals don't queue, so a second
        // raise would merge, which isn't what's counted here.
        if pending.fetch_or(bit(signal), Ordering::AcqRel) & bit(signal) == 0 {
            raised_each[signal as usize - 1] += 1;
            raised.fetch_add(1, Ordering::Relaxed);
        }
    }
    done.store(true, Ordering::Release);
    for claimer in claimers {
        claimer.join().unwrap();
    }
    assert_eq!(pending.load(Ordering::Acquire), 0);
    for signal in 0..64 {
        assert_eq!(taken[signal].load(Ordering::Acquire), raised_each[signal], "signal {}", signal + 1);
    }
    assert!(raised.load(Ordering::Relaxed) > 64);
    // Nothing unblocked, nothing claimed.
    let word = AtomicU64::new(bit(sig::TERM));
    assert_eq!(claim(&word, bit(sig::TERM)), None);
    assert_eq!(claim(&word, 0), Some(sig::TERM));
    assert_eq!(claim(&word, 0), None);
}

#[test]
fn restart_rewinds_by_port() {
    // x86_64: back over `int $0xf0`, and the number where the result went; every other
    // register, the arguments among them, as the call found them.
    let mut regs = x86_regs();
    let before = regs;
    regs.rax = (-(feos_abi::errno::EINTR as i64)) as u64;
    x86_64::rewind(&mut regs, feos_abi::nr::READ as u64);
    assert_eq!(regs.rip, before.rip - 2);
    assert_eq!(regs.rax, feos_abi::nr::READ as u64);
    assert_eq!((regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9), (before.rdi, before.rsi, before.rdx, before.r10, before.r8, before.r9));
    assert_eq!((regs.rsp, regs.rflags, regs.cs), (before.rsp, before.rflags, before.cs));
    assert_eq!(x86_64::SYSCALL_LENGTH, 2);

    // AArch64: back over `svc #0`, and the first argument where the result went; the
    // number is still in `x8`.
    let mut regs = arm_regs();
    let before = regs;
    regs.x[0] = (-(feos_abi::errno::EINTR as i64)) as u64;
    aarch64::rewind(&mut regs, 7);
    assert_eq!(regs.pc, before.pc - 4);
    assert_eq!(regs.x[0], 7);
    assert_eq!(regs.x[1..], before.x[1..]);
    assert_eq!((regs.sp, regs.pstate), (before.sp, before.pstate));
    assert_eq!(aarch64::SYSCALL_LENGTH, 4);

    // A handler's frame built from the rewound registers saves the trap's address, so
    // the handler's `SIGRETURN` makes the call again.
    let mut regs = x86_regs();
    x86_64::rewind(&mut regs, feos_abi::nr::WAIT4 as u64);
    let mut out = [0u8; x86_64::FRAME_SIZE];
    x86_64::build(&regs, &entry(), &mut out).unwrap();
    let context = x86_context(&out);
    assert_eq!(word(&context, offset_of!(ucontext::X86_64, gregs) + greg::RIP * 8), x86_regs().rip - 2);
    assert_eq!(word(&context, offset_of!(ucontext::X86_64, gregs) + greg::RAX * 8), feos_abi::nr::WAIT4 as u64);
}
