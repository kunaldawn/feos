//! Runs through signal handlers (stage 1c.9): `SIGACTION`, the frames a handler is
//! entered with, `SIGRETURN`, faults as signals, `SIGPIPE` and `SIGCHLD`; and stage
//! 1c.10's masks, `SIGPENDING`, `SIGSUSPEND`, alternate stacks, calls a signal
//! interrupts or makes again, `THREAD_KILL`, which thread takes a process signal, and
//! `SETITIMER`. Reports each check, and exits zero if all passed.
//!
//! Its children are itself, in one of the `--child` modes below, each reporting in its
//! exit status.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

use feos_user::abi::{clock, errno, how, map, prot, sa, si, sig, spawn as spawn_flags, ss, wait, SigInfo, SigStack, Timespec, UContext};
use feos_user::signal::{self, block, disposition, getitimer, handle, mask, set_disposition, set_pc, setitimer, sigaltstack, sigpending, sigprocmask, sigsuspend, thread_kill, timer_us, unblock};
use feos_user::{close, entry, getcpu, gettid, getpid, getuid, kill, nanosleep, now_ns, pipe2, println, read, sleep_ms, spawn, thread, wait4, write, yield_now, Args};

const PROGRAM: &str = "/bin/sigtest";

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, good: bool, detail: core::fmt::Arguments<'_>) {
        if good {
            println!("sigtest {name}: ok ({detail})");
        } else {
            println!("sigtest {name}: FAILED ({detail})");
            self.failed += 1;
        }
    }
}

/// What the last handler call saw.
static HITS: AtomicU32 = AtomicU32::new(0);
static SIGNO: AtomicI32 = AtomicI32::new(0);
static CODE: AtomicI32 = AtomicI32::new(0);
static PID: AtomicI32 = AtomicI32::new(0);
static UID: AtomicU32 = AtomicU32::new(0);
static STATUS: AtomicI32 = AtomicI32::new(0);
static ADDRESS: AtomicU64 = AtomicU64::new(0);
/// Where a fault's handler sends the program on, past the instruction that faulted; 0
/// to return to it as it is.
static RESUME: AtomicU64 = AtomicU64::new(0);

fn reset() {
    HITS.store(0, Ordering::SeqCst);
    SIGNO.store(0, Ordering::SeqCst);
    CODE.store(-1, Ordering::SeqCst);
    ADDRESS.store(u64::MAX, Ordering::SeqCst);
}

extern "C" fn record(signo: i32, info: *mut SigInfo, context: *mut UContext) {
    // SAFETY: the kernel passes a `SigInfo` and a context on the handler's stack.
    let info = unsafe { *info };
    SIGNO.store(signo, Ordering::SeqCst);
    CODE.store(info.code, Ordering::SeqCst);
    PID.store(info.pid(), Ordering::SeqCst);
    UID.store(info.uid(), Ordering::SeqCst);
    STATUS.store(info.status(), Ordering::SeqCst);
    ADDRESS.store(info.address(), Ordering::SeqCst);
    let resume = RESUME.swap(0, Ordering::SeqCst);
    if resume != 0 {
        // SAFETY: as above.
        unsafe { set_pc(context, resume) };
    }
    HITS.fetch_add(1, Ordering::SeqCst);
}

fn me() -> isize {
    getpid()
}

fn test_handler_runs(report: &mut Report) {
    reset();
    let installed = handle(sig::USR1, record, sa::SIGINFO);
    let sent = kill(me(), sig::USR1);
    // The handler ran as `kill`'s call returned, before this line.
    let hits = HITS.load(Ordering::SeqCst);
    report.check("handler-runs", installed == 0 && sent == 0 && hits == 1, format_args!("sigaction={installed} kill={sent} hits={hits}"));
    let (pid, uid, code) = (PID.load(Ordering::SeqCst), UID.load(Ordering::SeqCst), CODE.load(Ordering::SeqCst));
    report.check(
        "siginfo-pid-uid",
        SIGNO.load(Ordering::SeqCst) == sig::USR1 as i32 && pid as isize == me() && uid as isize == getuid() && code == si::USER,
        format_args!("signo={} pid={pid} uid={uid} code={code}", SIGNO.load(Ordering::SeqCst)),
    );
    set_disposition(sig::USR1, sa::DFL);
}

static SEEN_PC: AtomicU64 = AtomicU64::new(0);
static SEEN_SP: AtomicU64 = AtomicU64::new(0);
static SEEN_MASK: AtomicU64 = AtomicU64::new(u64::MAX);
static SEEN_RESULT: AtomicU64 = AtomicU64::new(u64::MAX);
static SEEN_EXTRA: AtomicU64 = AtomicU64::new(0);

extern "C" fn inspect(_: i32, _: *mut SigInfo, context: *mut UContext) {
    // SAFETY: the kernel's context, on this handler's stack.
    let context = unsafe { &*context };
    // SAFETY: as above.
    let (pc, sp) = unsafe { (signal::pc(context as *const UContext as *mut UContext), signal::sp(context as *const UContext as *mut UContext)) };
    SEEN_PC.store(pc, Ordering::SeqCst);
    SEEN_SP.store(sp, Ordering::SeqCst);
    SEEN_MASK.store(context.mask, Ordering::SeqCst);
    #[cfg(target_arch = "x86_64")]
    {
        use feos_user::abi::ucontext::greg;
        SEEN_RESULT.store(context.gregs[greg::RAX], Ordering::SeqCst);
        // The trap a call comes in by, and the FP area's address for the handler.
        let fx_ok = context.fpregs == context as *const UContext as u64 + 304 && context.flags == 1;
        SEEN_EXTRA.store(context.gregs[greg::TRAPNO] << 1 | u64::from(fx_ok), Ordering::SeqCst);
    }
    #[cfg(target_arch = "aarch64")]
    {
        SEEN_RESULT.store(context.regs[0], Ordering::SeqCst);
        SEEN_EXTRA.store(u64::from(context.flags == 0 && context.regs[8] == feos_user::abi::nr::KILL as u64), Ordering::SeqCst);
    }
    HITS.fetch_add(1, Ordering::SeqCst);
}

fn test_ucontext(report: &mut Report) {
    reset();
    handle(sig::USR1, inspect, 0);
    let here = feos_user::stack_pointer() as u64;
    kill(me(), sig::USR1);
    set_disposition(sig::USR1, sa::DFL);
    let (pc, sp, mask, result, extra) = (
        SEEN_PC.load(Ordering::SeqCst),
        SEEN_SP.load(Ordering::SeqCst),
        SEEN_MASK.load(Ordering::SeqCst),
        SEEN_RESULT.load(Ordering::SeqCst),
        SEEN_EXTRA.load(Ordering::SeqCst),
    );
    let code = feos_user::syscall as *const () as u64;
    // The interrupted call's program counter is in this program's code, its stack pointer
    // just below this function's, its result (0) already in place, and the mask the one
    // before the handler: nothing blocked.
    #[cfg(target_arch = "x86_64")]
    let extra_ok = extra == (0xf0 << 1 | 1);
    #[cfg(target_arch = "aarch64")]
    let extra_ok = extra == 1;
    let good = pc.abs_diff(code) < 1 << 20 && sp <= here && here - sp < 1 << 16 && mask == 0 && result == 0 && extra_ok;
    report.check("ucontext-has-registers", HITS.load(Ordering::SeqCst) == 1 && good, format_args!("pc={pc:#x} sp={sp:#x} here={here:#x} mask={mask:#x} result={result} extra={extra:#x}"));
}

/// Every register the program can see, before a call a handler interrupts and after.
#[cfg(target_arch = "aarch64")]
#[repr(C, align(16))]
struct Registers {
    before: [u64; 32],
    after: [u64; 32],
    vbefore: [u128; 32],
    vafter: [u128; 32],
    nzcv: u64,
    fpcr_after: u64,
    fpcr_in: u64,
    pad: u64,
}

#[cfg(target_arch = "x86_64")]
#[repr(C, align(16))]
struct Registers {
    before: [u64; 16],
    after: [u64; 16],
    xbefore: [u128; 16],
    xafter: [u128; 16],
    cf: u64,
    mxcsr_in: u32,
    mxcsr_out: u32,
    mxcsr_default: u32,
    pad: [u32; 3],
}

/// A handler that leaves nothing as it found it: every general and FP register it may
/// write, the flags, and the FP control register.
extern "C" fn clobber(_: i32, _: *mut SigInfo, _: *mut UContext) {
    // SAFETY: only registers, each declared clobbered.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!(
            "mov x9, #0x5555", "mov x10, x9", "mov x11, x9", "mov x12, x9", "mov x13, x9", "mov x14, x9",
            "mov x15, x9", "mov x16, x9", "mov x17, x9", "mov x20, x9", "mov x21, x9", "mov x22, x9",
            "mov x23, x9", "mov x24, x9", "mov x25, x9", "mov x26, x9", "mov x27, x9", "mov x28, x9",
            "movi v0.16b, #0x77", "movi v1.16b, #0x77", "movi v2.16b, #0x77", "movi v3.16b, #0x77",
            "movi v8.16b, #0x77", "movi v15.16b, #0x77", "movi v16.16b, #0x77", "movi v31.16b, #0x77",
            "mov x9, #0xc00000", "msr fpcr, x9", "cmn x9, #1", "msr fpcr, xzr",
            out("x9") _, out("x10") _, out("x11") _, out("x12") _, out("x13") _, out("x14") _, out("x15") _,
            out("x16") _, out("x17") _, out("x20") _, out("x21") _, out("x22") _, out("x23") _, out("x24") _,
            out("x25") _, out("x26") _, out("x27") _, out("x28") _, out("v0") _, out("v1") _, out("v2") _,
            out("v3") _, out("v8") _, out("v15") _, out("v16") _, out("v31") _,
        );
    }
    // SAFETY: as above.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let mxcsr: u32 = 0x1f80 | 1 << 13;
        core::arch::asm!(
            "mov rcx, -1", "mov rdx, rcx", "mov r8, rcx", "mov r9, rcx", "mov r10, rcx", "mov r11, rcx",
            "mov r13, rcx", "mov r14, rcx", "mov r15, rcx",
            "pcmpeqd xmm0, xmm0", "pcmpeqd xmm1, xmm1", "pcmpeqd xmm7, xmm7", "pcmpeqd xmm8, xmm8",
            "pcmpeqd xmm15, xmm15", "ldmxcsr [{mxcsr}]", "clc",
            mxcsr = in(reg) &mxcsr,
            out("rcx") _, out("rdx") _, out("r8") _, out("r9") _, out("r10") _, out("r11") _, out("r13") _,
            out("r14") _, out("r15") _, out("xmm0") _, out("xmm1") _, out("xmm7") _, out("xmm8") _, out("xmm15") _,
        );
    }
    HITS.fetch_add(1, Ordering::SeqCst);
}

/// `KILL` of this process with `signal`, every register set from `state.before` around
/// it, and what they were after it in `state.after`.
#[cfg(target_arch = "aarch64")]
fn kill_keeping(state: &mut Registers, signal: u32) -> isize {
    let result: isize;
    // SAFETY: only registers and `state`, which `x2` points at; every register written is
    // declared.
    unsafe {
        core::arch::asm!(
            "mov x3, x2",
            "ldr x4, [x3, #1552]",
            "msr fpcr, x4",
            "add x5, x3, #512",
            "ldp q0, q1, [x5, #0]", "ldp q2, q3, [x5, #32]", "ldp q4, q5, [x5, #64]", "ldp q6, q7, [x5, #96]",
            "ldp q8, q9, [x5, #128]", "ldp q10, q11, [x5, #160]", "ldp q12, q13, [x5, #192]", "ldp q14, q15, [x5, #224]",
            "ldp q16, q17, [x5, #256]", "ldp q18, q19, [x5, #288]", "ldp q20, q21, [x5, #320]", "ldp q22, q23, [x5, #352]",
            "ldp q24, q25, [x5, #384]", "ldp q26, q27, [x5, #416]", "ldp q28, q29, [x5, #448]", "ldp q30, q31, [x5, #480]",
            "ldr x4, [x3, #32]", "ldr x5, [x3, #40]", "ldr x6, [x3, #48]", "ldr x7, [x3, #56]",
            "ldr x9, [x3, #72]", "ldr x10, [x3, #80]", "ldr x11, [x3, #88]", "ldr x12, [x3, #96]",
            "ldr x13, [x3, #104]", "ldr x14, [x3, #112]", "ldr x15, [x3, #120]", "ldr x16, [x3, #128]",
            "ldr x17, [x3, #136]", "ldr x20, [x3, #160]", "ldr x21, [x3, #168]", "ldr x22, [x3, #176]",
            "ldr x23, [x3, #184]", "ldr x24, [x3, #192]", "ldr x25, [x3, #200]", "ldr x26, [x3, #208]",
            "ldr x27, [x3, #216]", "ldr x28, [x3, #224]",
            // Z and C set, N and V clear.
            "cmp x3, x3",
            "svc #0",
            "str x4, [x3, #288]", "str x5, [x3, #296]", "str x6, [x3, #304]", "str x7, [x3, #312]",
            "str x9, [x3, #328]", "str x10, [x3, #336]", "str x11, [x3, #344]", "str x12, [x3, #352]",
            "str x13, [x3, #360]", "str x14, [x3, #368]", "str x15, [x3, #376]", "str x16, [x3, #384]",
            "str x17, [x3, #392]", "str x20, [x3, #416]", "str x21, [x3, #424]", "str x22, [x3, #432]",
            "str x23, [x3, #440]", "str x24, [x3, #448]", "str x25, [x3, #456]", "str x26, [x3, #464]",
            "str x27, [x3, #472]", "str x28, [x3, #480]", "str x2, [x3, #272]", "str x1, [x3, #264]",
            "mrs x4, nzcv", "str x4, [x3, #1536]",
            "mrs x4, fpcr", "str x4, [x3, #1544]",
            "msr fpcr, xzr",
            "add x5, x3, #1024",
            "stp q0, q1, [x5, #0]", "stp q2, q3, [x5, #32]", "stp q4, q5, [x5, #64]", "stp q6, q7, [x5, #96]",
            "stp q8, q9, [x5, #128]", "stp q10, q11, [x5, #160]", "stp q12, q13, [x5, #192]", "stp q14, q15, [x5, #224]",
            "stp q16, q17, [x5, #256]", "stp q18, q19, [x5, #288]", "stp q20, q21, [x5, #320]", "stp q22, q23, [x5, #352]",
            "stp q24, q25, [x5, #384]", "stp q26, q27, [x5, #416]", "stp q28, q29, [x5, #448]", "stp q30, q31, [x5, #480]",
            inlateout("x0") me() => result,
            in("x1") signal as usize,
            in("x2") state as *mut Registers,
            in("x8") feos_user::abi::nr::KILL,
            out("x3") _, out("x4") _, out("x5") _, out("x6") _, out("x7") _, out("x9") _, out("x10") _,
            out("x11") _, out("x12") _, out("x13") _, out("x14") _, out("x15") _, out("x16") _, out("x17") _,
            out("x20") _, out("x21") _, out("x22") _, out("x23") _, out("x24") _, out("x25") _, out("x26") _,
            out("x27") _, out("x28") _,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _, out("v6") _, out("v7") _,
            out("v8") _, out("v9") _, out("v10") _, out("v11") _, out("v12") _, out("v13") _, out("v14") _, out("v15") _,
            out("v16") _, out("v17") _, out("v18") _, out("v19") _, out("v20") _, out("v21") _, out("v22") _, out("v23") _,
            out("v24") _, out("v25") _, out("v26") _, out("v27") _, out("v28") _, out("v29") _, out("v30") _, out("v31") _,
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
fn kill_keeping(state: &mut Registers, signal: u32) -> isize {
    let result: isize;
    // SAFETY: only registers and `state`, which `r12` points at; `rbx` and `rbp` are put
    // back as they were, and every other register written is declared.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "push rbp",
            "ldmxcsr [r12 + 776]",
            "movdqu xmm0, [r12 + 256]", "movdqu xmm1, [r12 + 272]", "movdqu xmm2, [r12 + 288]", "movdqu xmm3, [r12 + 304]",
            "movdqu xmm4, [r12 + 320]", "movdqu xmm5, [r12 + 336]", "movdqu xmm6, [r12 + 352]", "movdqu xmm7, [r12 + 368]",
            "movdqu xmm8, [r12 + 384]", "movdqu xmm9, [r12 + 400]", "movdqu xmm10, [r12 + 416]", "movdqu xmm11, [r12 + 432]",
            "movdqu xmm12, [r12 + 448]", "movdqu xmm13, [r12 + 464]", "movdqu xmm14, [r12 + 480]", "movdqu xmm15, [r12 + 496]",
            "mov rbx, [r12 + 0]", "mov rcx, [r12 + 8]", "mov rdx, [r12 + 16]", "mov rbp, [r12 + 24]",
            "mov r8, [r12 + 32]", "mov r9, [r12 + 40]", "mov r10, [r12 + 48]", "mov r11, [r12 + 56]",
            "mov r13, [r12 + 64]", "mov r14, [r12 + 72]", "mov r15, [r12 + 80]",
            "stc",
            "int 0xf0",
            "setc byte ptr [r12 + 768]",
            "mov [r12 + 128], rbx", "mov [r12 + 136], rcx", "mov [r12 + 144], rdx", "mov [r12 + 152], rbp",
            "mov [r12 + 160], r8", "mov [r12 + 168], r9", "mov [r12 + 176], r10", "mov [r12 + 184], r11",
            "mov [r12 + 192], r13", "mov [r12 + 200], r14", "mov [r12 + 208], r15", "mov [r12 + 216], rdi",
            "mov [r12 + 224], rsi",
            "stmxcsr [r12 + 780]",
            "ldmxcsr [r12 + 784]",
            "movdqu [r12 + 512], xmm0", "movdqu [r12 + 528], xmm1", "movdqu [r12 + 544], xmm2", "movdqu [r12 + 560], xmm3",
            "movdqu [r12 + 576], xmm4", "movdqu [r12 + 592], xmm5", "movdqu [r12 + 608], xmm6", "movdqu [r12 + 624], xmm7",
            "movdqu [r12 + 640], xmm8", "movdqu [r12 + 656], xmm9", "movdqu [r12 + 672], xmm10", "movdqu [r12 + 688], xmm11",
            "movdqu [r12 + 704], xmm12", "movdqu [r12 + 720], xmm13", "movdqu [r12 + 736], xmm14", "movdqu [r12 + 752], xmm15",
            "pop rbp",
            "pop rbx",
            inlateout("rax") feos_user::abi::nr::KILL as isize => result,
            in("rdi") me(),
            in("rsi") signal as usize,
            in("r12") state as *mut Registers,
            out("rcx") _, out("rdx") _, out("r8") _, out("r9") _, out("r10") _, out("r11") _, out("r13") _,
            out("r14") _, out("r15") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _, out("xmm4") _, out("xmm5") _, out("xmm6") _,
            out("xmm7") _, out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _, out("xmm12") _, out("xmm13") _,
            out("xmm14") _, out("xmm15") _,
        );
    }
    result
}

fn test_registers_survive(report: &mut Report) {
    reset();
    handle(sig::USR2, clobber, 0);
    // SAFETY: integers and arrays of them, all valid as zero.
    let mut state: Registers = unsafe { core::mem::zeroed() };
    #[cfg(target_arch = "aarch64")]
    {
        for (index, value) in state.before.iter_mut().enumerate() {
            *value = 0x1111_0000_0000_0000 | (index as u64) << 8 | index as u64;
        }
        for (index, value) in state.vbefore.iter_mut().enumerate() {
            *value = (0xabcd_0000_u128 | index as u128) << 64 | 0x1234_5678_0000 | index as u128;
        }
        // Rounding towards plus infinity.
        state.fpcr_in = 1 << 22;
    }
    #[cfg(target_arch = "x86_64")]
    {
        for (index, value) in state.before.iter_mut().enumerate() {
            *value = 0x2222_0000_0000_0000 | (index as u64) << 8 | index as u64;
        }
        for (index, value) in state.xbefore.iter_mut().enumerate() {
            *value = (0xdcba_0000_u128 | index as u128) << 64 | 0x8765_4321_0000 | index as u128;
        }
        // Rounding towards zero.
        state.mxcsr_in = 0x1f80 | 3 << 13;
        state.mxcsr_default = 0x1f80;
    }
    let result = kill_keeping(&mut state, sig::USR2);
    set_disposition(sig::USR2, sa::DFL);
    let hits = HITS.load(Ordering::SeqCst);
    #[cfg(target_arch = "aarch64")]
    let (general, fp, extra) = {
        let checked = (4..8).chain(9..18).chain(20..29);
        let general = checked.clone().all(|index| state.after[index] == state.before[index])
            && state.after[2] == &state as *const Registers as u64
            && state.after[1] == u64::from(sig::USR2);
        let fp = state.vafter == state.vbefore && state.fpcr_after == state.fpcr_in;
        (general, fp, state.nzcv == 0x6000_0000)
    };
    #[cfg(target_arch = "x86_64")]
    let (general, fp, extra) = {
        let general = (0..11).all(|index| state.after[index] == state.before[index])
            && state.after[11] == me() as u64
            && state.after[12] == u64::from(sig::USR2);
        let fp = state.xafter == state.xbefore && state.mxcsr_out == state.mxcsr_in;
        (general, fp, state.cf & 0xff == 1)
    };
    report.check(
        "sigreturn-restores-registers",
        result == 0 && hits == 1 && general && fp && extra,
        format_args!("kill={result} hits={hits} general={general} fp={fp} flags={extra}"),
    );
}

static DEPTH: AtomicU32 = AtomicU32::new(0);
static MOST: AtomicU32 = AtomicU32::new(0);
static CALLS: AtomicU32 = AtomicU32::new(0);

extern "C" fn nester(signo: i32, _: *mut SigInfo, _: *mut UContext) {
    let depth = DEPTH.fetch_add(1, Ordering::SeqCst) + 1;
    MOST.fetch_max(depth, Ordering::SeqCst);
    if CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
        // The same signal again, from inside its own handler.
        kill(me(), signo as u32);
    }
    DEPTH.fetch_sub(1, Ordering::SeqCst);
}

fn test_flags(report: &mut Report) {
    reset();
    handle(sig::USR2, record, sa::RESETHAND);
    kill(me(), sig::USR2);
    let after = disposition(sig::USR2).map(|action| action.handler);
    report.check(
        "sa-resethand",
        HITS.load(Ordering::SeqCst) == 1 && after == Ok(sa::DFL),
        format_args!("hits={} then handler={after:?}", HITS.load(Ordering::SeqCst)),
    );
    // Without `SA_NODEFER` the second waits until the first returns; with it, it runs
    // inside the first.
    let mut seen = [(0, 0); 2];
    for (index, flags) in [0, sa::NODEFER].into_iter().enumerate() {
        DEPTH.store(0, Ordering::SeqCst);
        MOST.store(0, Ordering::SeqCst);
        CALLS.store(0, Ordering::SeqCst);
        handle(sig::USR1, nester, flags);
        kill(me(), sig::USR1);
        seen[index] = (CALLS.load(Ordering::SeqCst), MOST.load(Ordering::SeqCst));
    }
    set_disposition(sig::USR1, sa::DFL);
    report.check("sa-nodefer", seen == [(2, 1), (2, 2)], format_args!("(calls, deepest) deferred {:?} nodefer {:?}", seen[0], seen[1]));
}

/// Takes a fault of the kind `kind` names, with the handler for `signal` sending the
/// program on past it. The signal, code and address the handler saw.
fn fault_and_skip(signal: u32, kind: &str) -> (u32, i32, i32, u64) {
    reset();
    handle(signal, record, 0);
    let slot = RESUME.as_ptr();
    // SAFETY: each faults at once and the handler moves the program on to `2:`; the
    // registers written are declared.
    unsafe {
        use core::arch::asm;
        #[cfg(target_arch = "aarch64")]
        match kind {
            "null" => asm!("adr {t}, 2f", "str {t}, [{slot}]", "mov {z}, #0", "ldr {z}, [{z}]", "2:", t = out(reg) _, z = out(reg) _, slot = in(reg) slot),
            "ill" => asm!("adr {t}, 2f", "str {t}, [{slot}]", "udf #0", "2:", t = out(reg) _, slot = in(reg) slot),
            "trap" => asm!("adr {t}, 2f", "str {t}, [{slot}]", "brk #0", "2:", t = out(reg) _, slot = in(reg) slot),
            _ => {}
        }
        #[cfg(target_arch = "x86_64")]
        match kind {
            "null" => asm!("lea {t}, [rip + 2f]", "mov [{slot}], {t}", "xor {z:e}, {z:e}", "mov {z}, [{z}]", "2:", t = out(reg) _, z = out(reg) _, slot = in(reg) slot),
            "ill" => asm!("lea {t}, [rip + 2f]", "mov [{slot}], {t}", "ud2", "2:", t = out(reg) _, slot = in(reg) slot),
            "trap" => asm!("lea {t}, [rip + 2f]", "mov [{slot}], {t}", "int3", "2:", t = out(reg) _, slot = in(reg) slot),
            "divide" => asm!(
                "lea {t}, [rip + 2f]", "mov [{slot}], {t}", "xor edx, edx", "xor ecx, ecx", "mov eax, 1", "div ecx", "2:",
                t = out(reg) _, slot = in(reg) slot, out("rax") _, out("rcx") _, out("rdx") _,
            ),
            _ => {}
        }
    }
    RESUME.store(0, Ordering::SeqCst);
    set_disposition(signal, sa::DFL);
    (HITS.load(Ordering::SeqCst), SIGNO.load(Ordering::SeqCst), CODE.load(Ordering::SeqCst), ADDRESS.load(Ordering::SeqCst))
}

fn test_faults(report: &mut Report) {
    let (hits, signo, code, address) = fault_and_skip(sig::SEGV, "null");
    report.check(
        "segv-handler-skips-instruction",
        hits == 1 && signo == sig::SEGV as i32 && code == si::SEGV_MAPERR && address == 0,
        format_args!("hits={hits} signo={signo} code={code} address={address:#x}"),
    );
    let (hits, signo, code, address) = fault_and_skip(sig::ILL, "ill");
    report.check("sigill", hits == 1 && signo == sig::ILL as i32 && code == si::ILL_ILLOPC, format_args!("hits={hits} signo={signo} code={code} address={address:#x}"));
    let (hits, signo, code, address) = fault_and_skip(sig::TRAP, "trap");
    report.check("sigtrap", hits == 1 && signo == sig::TRAP as i32 && code == si::TRAP_BRKPT, format_args!("hits={hits} signo={signo} code={code} address={address:#x}"));
    #[cfg(target_arch = "x86_64")]
    {
        let (hits, signo, code, address) = fault_and_skip(sig::FPE, "divide");
        report.check("sigfpe", hits == 1 && signo == sig::FPE as i32 && code == si::FPE_INTDIV, format_args!("hits={hits} signo={signo} code={code} address={address:#x}"));
    }
    #[cfg(target_arch = "aarch64")]
    {
        reset();
        handle(sig::FPE, record, 0);
        let quotient: u64;
        // SAFETY: a division, nothing else.
        unsafe { core::arch::asm!("udiv {q}, {a}, {z}", q = out(reg) quotient, a = in(reg) 7u64, z = in(reg) core::hint::black_box(0u64)) };
        set_disposition(sig::FPE, sa::DFL);
        let hits = HITS.load(Ordering::SeqCst);
        report.check("divide-by-zero-no-signal", quotient == 0 && hits == 0, format_args!("7/0={quotient} signals={hits}"));
    }
}

/// The status `pid` ended with, waited for.
fn reap(pid: isize) -> i32 {
    match wait4(pid, 0) {
        Ok((_, status)) => status,
        Err(error) => error as i32,
    }
}

fn test_sigpipe(report: &mut Report) {
    let child = spawn(PROGRAM, &[PROGRAM, "--child", "sigpipe-default"]);
    let status = reap(child);
    report.check(
        "sigpipe-default-kills",
        child > 0 && wait::signaled(status) && wait::term_signal(status) == sig::PIPE as i32,
        format_args!("pid={child} status={status:#x}"),
    );
    set_disposition(sig::PIPE, sa::IGN);
    let written = match pipe2(0) {
        Ok((reader, writer)) => {
            close(reader);
            let written = write(writer, b"x");
            close(writer);
            written
        }
        Err(error) => error,
    };
    set_disposition(sig::PIPE, sa::DFL);
    report.check("sigpipe-ignored-epipe", written == -errno::EPIPE, format_args!("write={written}"));
}

/// A child's: writes to a pipe nobody reads, which should end it with `SIGPIPE`.
fn child_sigpipe() -> i32 {
    let Ok((reader, writer)) = pipe2(0) else {
        return 2;
    };
    close(reader);
    write(writer, b"x");
    // Still here: the signal didn't end it.
    3
}

static REAPED: AtomicI32 = AtomicI32::new(0);
static CHILD_CODES: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_child(_: i32, info: *mut SigInfo, _: *mut UContext) {
    // SAFETY: the kernel's.
    let info = unsafe { *info };
    CODE.store(info.code, Ordering::SeqCst);
    PID.store(info.pid(), Ordering::SeqCst);
    STATUS.store(info.status(), Ordering::SeqCst);
    if info.code >= 0 && info.code < 32 {
        CHILD_CODES.fetch_or(1 << info.code, Ordering::SeqCst);
    }
    if info.code == si::CLD_EXITED || info.code == si::CLD_KILLED {
        // What `SIGCHLD` says has ended is there to reap.
        if let Ok((pid, _)) = wait4(info.pid() as isize, wait::NOHANG) {
            REAPED.store(pid as i32, Ordering::SeqCst);
        }
    }
    HITS.fetch_add(1, Ordering::SeqCst);
}

/// Waits up to `ms` for `done` to hold.
fn wait_until(ms: u64, done: impl Fn() -> bool) -> bool {
    let start = now_ns(clock::MONOTONIC);
    while !done() {
        if now_ns(clock::MONOTONIC) - start > ms * 1_000_000 {
            return false;
        }
        sleep_ms(2);
    }
    true
}

fn test_sigchld(report: &mut Report) {
    reset();
    REAPED.store(0, Ordering::SeqCst);
    handle(sig::CHLD, on_child, 0);
    let child = spawn("/bin/hello", &["/bin/hello", "--exit", "3", "--quiet"]);
    let reaped = wait_until(5000, || REAPED.load(Ordering::SeqCst) != 0);
    let again = wait4(child, wait::NOHANG);
    report.check(
        "sigchld-handler-finds-record",
        reaped && REAPED.load(Ordering::SeqCst) as isize == child && CODE.load(Ordering::SeqCst) == si::CLD_EXITED && STATUS.load(Ordering::SeqCst) == 3 && again == Err(-errno::ECHILD),
        format_args!("pid={child} reaped={} code={} status={} again={again:?}", REAPED.load(Ordering::SeqCst), CODE.load(Ordering::SeqCst), STATUS.load(Ordering::SeqCst)),
    );

    // Stops and continues are reported, and not with `SA_NOCLDSTOP`.
    CHILD_CODES.store(0, Ordering::SeqCst);
    let child = spawn(PROGRAM, &[PROGRAM, "--child", "spin"]);
    sleep_ms(50);
    kill(child, sig::STOP);
    let stopped = wait_until(3000, || CHILD_CODES.load(Ordering::SeqCst) & 1 << si::CLD_STOPPED != 0);
    kill(child, sig::CONT);
    let continued = wait_until(3000, || CHILD_CODES.load(Ordering::SeqCst) & 1 << si::CLD_CONTINUED != 0);
    signal::sigaction(sig::CHLD, Some(&signal::action(on_child, sa::NOCLDSTOP, 0)), None);
    CHILD_CODES.store(0, Ordering::SeqCst);
    kill(child, sig::STOP);
    // The stop itself is still there for `WUNTRACED`.
    let reported = wait4(child, wait::UNTRACED).map(|(_, status)| status);
    sleep_ms(100);
    let quiet = CHILD_CODES.load(Ordering::SeqCst) & (1 << si::CLD_STOPPED | 1 << si::CLD_CONTINUED) == 0;
    kill(child, sig::CONT);
    sleep_ms(50);
    let quiet = quiet && CHILD_CODES.load(Ordering::SeqCst) & 1 << si::CLD_CONTINUED == 0;
    kill(child, sig::KILL);
    let killed = wait_until(3000, || CHILD_CODES.load(Ordering::SeqCst) & 1 << si::CLD_KILLED != 0);
    report.check(
        "sigchld-stop-reported",
        stopped && continued && quiet && killed && reported == Ok(0x137f),
        format_args!("stopped={stopped} continued={continued} quiet-with-nocldstop={quiet} killed={killed} wuntraced={reported:?}"),
    );
    // Reaped by the handler or not, it's gone.
    let _ = wait4(child, 0);

    // Explicitly ignored: nobody waits, and a wait finds nothing once it has ended.
    set_disposition(sig::CHLD, sa::IGN);
    let child = spawn("/bin/hello", &["/bin/hello", "--quiet"]);
    let waited = wait4(child, 0);
    set_disposition(sig::CHLD, sa::DFL);
    report.check("sigchld-ign-autoreaps", child > 0 && waited == Err(-errno::ECHILD), format_args!("pid={child} wait4={waited:?}"));

    // The default keeps it for `WAIT4`.
    let child = spawn("/bin/hello", &["/bin/hello", "--exit", "5", "--quiet"]);
    sleep_ms(100);
    let found = wait_until(3000, || matches!(wait4(child, wait::NOHANG), Ok((pid, _)) if pid == child));
    report.check("sigchld-default-keeps-records", child > 0 && found, format_args!("pid={child} reaped-later={found}"));

    // A `SIG_IGN` the program didn't set itself reaps all the same: one inherited from
    // its spawner, and one kept across `EXEC`. Each child reports whether a child of its
    // own was reaped for it. This one ignores `SIGCHLD` only for the spawn, and lets the
    // child end only once it's back at the default, which keeps the child's record.
    set_disposition(sig::CHLD, sa::IGN);
    let (child, status) = reap_check(&[PROGRAM, "--child", "reap-check"], || {
        set_disposition(sig::CHLD, sa::DFL);
    });
    report.check("sigchld-ign-inherited-autoreaps", child > 0 && status == 0, format_args!("pid={child} status={status:#x}"));
    let (child, status) = reap_check(&[PROGRAM, "--child", "exec-ignoring"], || {});
    report.check("sigchld-ign-kept-across-exec", child > 0 && status == 0, format_args!("pid={child} status={status:#x}"));
}

/// Spawns `args` with its standard input a pipe from here, runs `then`, lets the child
/// go on by writing to the pipe, and reaps it: its pid and status.
fn reap_check(args: &[&str], then: impl FnOnce()) -> (isize, i32) {
    let Ok((reader, writer)) = pipe2(feos_user::abi::o::CLOEXEC) else {
        then();
        return (-1, -1);
    };
    let child = feos_user::spawn_io(PROGRAM, args, reader, feos_user::abi::INHERIT);
    close(reader);
    then();
    write(writer, b"g");
    close(writer);
    (child, if child > 0 { reap(child) } else { -1 })
}

/// A child's: with `SIGCHLD` ignored (not set here), a child of its own ends unseen, so a
/// wait for it is `ECHILD`. Waits for its parent's go on standard input before it ends.
/// Its status is 0 if so: 1 if `SIGCHLD` wasn't ignored, 2 if the wait found the child.
fn child_reap_check() -> i32 {
    let ignored = disposition(sig::CHLD).is_ok_and(|action| action.handler == sa::IGN);
    let child = spawn("/bin/hello", &["/bin/hello", "--quiet"]);
    let waited = wait4(child, 0);
    let mut go = [0u8; 1];
    read(0, &mut go);
    match (ignored, child > 0 && waited == Err(-errno::ECHILD)) {
        (false, _) => 1,
        (true, false) => 2,
        (true, true) => 0,
    }
}

/// A child's: ignores `SIGCHLD` and runs itself again as `reap-check`.
fn child_exec_ignoring() -> i32 {
    set_disposition(sig::CHLD, sa::IGN);
    feos_user::exec_args(PROGRAM, &[PROGRAM, "--child", "reap-check"]);
    3
}

/// A child's: spins, sleeping a little, until it's killed.
fn child_spin() -> i32 {
    loop {
        sleep_ms(5);
    }
}

static HANDLED_AT: AtomicU64 = AtomicU64::new(0);

extern "C" fn stamp(_: i32, _: *mut SigInfo, _: *mut UContext) {
    HANDLED_AT.store(now_ns(clock::MONOTONIC), Ordering::SeqCst);
}

/// A child's: a call that waits 300 ms, which the parent signals part way, 50 ms in. The
/// signal cuts the wait short (stage 1c.10: `EINTR`, with what was left in `remain`), and
/// the handler runs as the call returns: well before the 300 ms, and well within a tick
/// of the call's return. Its status is 0 if so. Its standard output is a pipe to the
/// parent, which it says it's ready on, and then when what happened.
fn child_call_return() -> i32 {
    handle(sig::USR1, stamp, 0);
    write(1, b"r");
    let start = now_ns(clock::MONOTONIC);
    let request = Timespec { sec: 0, nsec: 300_000_000 };
    let mut remain = Timespec::default();
    let result = nanosleep(clock::MONOTONIC, 0, &request, Some(&mut remain));
    let returned = now_ns(clock::MONOTONIC);
    let handled = HANDLED_AT.load(Ordering::SeqCst);
    let left = remain.sec as u64 * 1_000_000_000 + remain.nsec as u64;
    println!(
        "nanosleep={result} slept={}us handled-after-start={}us returned-after-handler={}us remain={}us",
        (returned - start) / 1000,
        handled.saturating_sub(start) / 1000,
        returned.saturating_sub(handled) / 1000,
        left / 1000
    );
    let early = returned - start < 250_000_000 && left > 0 && left <= 300_000_000;
    if result == -errno::EINTR && early && handled >= start && returned >= handled && returned - handled < 2_000_000 {
        0
    } else {
        1
    }
}

fn test_call_return(report: &mut Report) {
    let Ok((reader, writer)) = pipe2(feos_user::abi::o::CLOEXEC) else {
        report.check("signal-on-call-return", false, format_args!("no pipe"));
        return;
    };
    let child = feos_user::spawn_io(PROGRAM, &[PROGRAM, "--child", "call-return"], feos_user::abi::INHERIT, writer);
    close(writer);
    let mut said = [0u8; 160];
    let got = read(reader, &mut said[..1]);
    sleep_ms(50);
    let sent = kill(child, sig::USR1);
    let mut length = 0;
    while length < said.len() {
        let more = read(reader, &mut said[length..]);
        if more <= 0 {
            break;
        }
        length += more as usize;
    }
    let status = reap(child);
    close(reader);
    let text = core::str::from_utf8(&said[..length]).unwrap_or("?").trim_end();
    report.check("signal-on-call-return", got == 1 && sent == 0 && status == 0, format_args!("pid={child} kill={sent} status={status:#x} {text}"));
}

/// Sleeps `ms`, through any signal that cuts a `NANOSLEEP` short.
fn sleep_through(ms: u64) {
    let end = now_ns(clock::MONOTONIC) + ms * 1_000_000;
    loop {
        let now = now_ns(clock::MONOTONIC);
        if now >= end {
            return;
        }
        nanosleep(clock::MONOTONIC, 0, &Timespec::from_nanos(end - now), None);
    }
}

const fn bit(signal: u32) -> u64 {
    1 << (signal - 1)
}

/// The thread a handler ran on, and how many have run.
static RAN_ON: AtomicU64 = AtomicU64::new(0);
static COUNT: AtomicU32 = AtomicU32::new(0);

extern "C" fn record_thread(signo: i32, info: *mut SigInfo, context: *mut UContext) {
    RAN_ON.store(gettid() as u64, Ordering::SeqCst);
    record(signo, info, context);
}

extern "C" fn count(_: i32, _: *mut SigInfo, _: *mut UContext) {
    COUNT.fetch_add(1, Ordering::SeqCst);
}

fn test_masks(report: &mut Report) {
    reset();
    handle(sig::USR1, record, 0);
    block(bit(sig::USR1));
    kill(me(), sig::USR1);
    kill(me(), sig::USR1);
    sleep_through(20);
    let held = HITS.load(Ordering::SeqCst);
    unblock(bit(sig::USR1));
    // Two sent while it was blocked are one when it's unblocked: they don't queue.
    let after = HITS.load(Ordering::SeqCst);
    report.check("mask-blocks-then-delivers", held == 0 && after == 1, format_args!("while blocked {held}, after {after}"));

    // The handler ran before `SIGPROCMASK` went back to the program, which sees it
    // done at once; the frame's mask, what the handler goes back to, is the new one.
    reset();
    handle(sig::USR1, inspect, 0);
    SEEN_MASK.store(u64::MAX, Ordering::SeqCst);
    sigprocmask(how::SETMASK, Some(bit(sig::USR1) | bit(sig::KILL)), None);
    let blocked = mask();
    kill(me(), sig::USR1);
    let before = HITS.load(Ordering::SeqCst);
    sigprocmask(how::SETMASK, Some(0), None);
    let hits = HITS.load(Ordering::SeqCst);
    let seen = SEEN_MASK.load(Ordering::SeqCst);
    set_disposition(sig::USR1, sa::DFL);
    report.check(
        "sigprocmask-unblock-delivers-before-return",
        blocked == bit(sig::USR1) && before == 0 && hits == 1 && seen == 0 && mask() == 0,
        format_args!("blocked={blocked:#x} (SIGKILL dropped) before={before} after={hits} frame-mask={seen:#x}"),
    );
}

fn test_sigpending(report: &mut Report) {
    // A process signal and a thread signal, both blocked, and both discarded by
    // `SIG_IGN`.
    block(bit(sig::USR2) | bit(sig::TERM));
    kill(me(), sig::USR2);
    thread_kill(gettid() as usize, sig::TERM);
    let before = sigpending();
    set_disposition(sig::USR2, sa::IGN);
    set_disposition(sig::TERM, sa::IGN);
    let after = sigpending();
    set_disposition(sig::USR2, sa::DFL);
    set_disposition(sig::TERM, sa::DFL);
    unblock(bit(sig::USR2) | bit(sig::TERM));
    report.check(
        "sigpending",
        before == Ok(bit(sig::USR2) | bit(sig::TERM)) && after == Ok(0),
        format_args!("pending {before:x?}, after SIG_IGN {after:x?}"),
    );
}

/// A thread's: after `arg` ms, `SIGUSR1` to the process.
fn send_later(arg: usize) -> usize {
    sleep_through(arg as u64);
    kill(getpid(), sig::USR1) as usize
}

fn test_sigsuspend(report: &mut Report) {
    reset();
    handle(sig::USR1, inspect, 0);
    // The sender starts with this mask, so only this thread can take the signal.
    block(bit(sig::USR1));
    let sender = thread::spawn(send_later, 30);
    let start = now_ns(clock::MONOTONIC);
    let result = sigsuspend(bit(sig::USR2));
    let waited = (now_ns(clock::MONOTONIC) - start) / 1000;
    let after = mask();
    let frame_mask = SEEN_MASK.load(Ordering::SeqCst);
    let joined = sender.map(thread::Thread::join);
    unblock(bit(sig::USR1));
    set_disposition(sig::USR1, sa::DFL);
    report.check(
        "sigsuspend",
        result == -errno::EINTR && HITS.load(Ordering::SeqCst) == 1 && after == bit(sig::USR1) && frame_mask == bit(sig::USR1) && joined == Ok(0),
        format_args!("result={result} waited={waited}us hits={} mask-after={after:#x} frame-mask={frame_mask:#x}", HITS.load(Ordering::SeqCst)),
    );
}

static HANDLER_SP: AtomicU64 = AtomicU64::new(0);
static HANDLER_FLAGS: AtomicI32 = AtomicI32::new(-1);

extern "C" fn where_am_i(_: i32, _: *mut SigInfo, _: *mut UContext) {
    let here = 0u8;
    HANDLER_SP.store(core::hint::black_box(&here) as *const u8 as u64, Ordering::SeqCst);
    let mut now = SigStack::default();
    sigaltstack(None, Some(&mut now));
    HANDLER_FLAGS.store(now.flags, Ordering::SeqCst);
    HITS.fetch_add(1, Ordering::SeqCst);
}

const ALT_SIZE: usize = 64 * 1024;

fn test_altstack(report: &mut Report) {
    let base = feos_user::mmap2(0, ALT_SIZE, prot::READ | prot::WRITE, map::PRIVATE);
    if base < 0 {
        report.check("altstack-used", false, format_args!("mmap2={base}"));
        return;
    }
    let base = base as u64;
    let stack = SigStack { sp: base, flags: 0, reserved: 0, size: ALT_SIZE as u64 };
    let set = sigaltstack(Some(&stack), None);
    let mut seen = SigStack::default();
    sigaltstack(None, Some(&mut seen));
    let mut runs = [(0u64, -1i32); 2];
    for (index, flags) in [sa::ONSTACK, 0].into_iter().enumerate() {
        reset();
        signal::sigaction(sig::USR1, Some(&signal::action(where_am_i, flags, 0)), None);
        kill(me(), sig::USR1);
        runs[index] = (HANDLER_SP.load(Ordering::SeqCst), HANDLER_FLAGS.load(Ordering::SeqCst));
    }
    set_disposition(sig::USR1, sa::DFL);
    let on = |sp: u64| sp >= base && sp < base + ALT_SIZE as u64;
    let disabled = sigaltstack(Some(&SigStack { flags: ss::DISABLE, ..SigStack::default() }), None);
    let mut gone = SigStack::default();
    sigaltstack(None, Some(&mut gone));
    feos_user::munmap(base as usize, ALT_SIZE);
    report.check(
        "altstack-used",
        set == 0
            && seen == stack
            && on(runs[0].0)
            && runs[0].1 == ss::ONSTACK
            && !on(runs[1].0)
            && runs[1].1 == 0
            && disabled == 0
            && gone.flags == ss::DISABLE,
        format_args!(
            "set={set} stack={base:#x}+{ALT_SIZE:#x} onstack-handler sp={:#x} flags={} plain sp={:#x} flags={} disabled-flags={}",
            runs[0].0, runs[0].1, runs[1].0, runs[1].1, gone.flags
        ),
    );
}

extern "C" fn on_overflow(_: i32, info: *mut SigInfo, _: *mut UContext) {
    use feos_user::abi::{USER_ARGS_SIZE, USER_STACK_GUARD, USER_STACK_SIZE, USER_STACK_TOP};
    // SAFETY: the kernel's, on the alternate stack.
    let address = unsafe { (*info).address() } as usize;
    let low = USER_STACK_TOP - USER_ARGS_SIZE - USER_STACK_SIZE;
    let in_guard = address < low && address >= low - USER_STACK_GUARD;
    println!("overflow si_addr={address:#x} stack-bottom={low:#x} in-guard={in_guard}");
    feos_user::exit(if in_guard { 0 } else { 1 });
}

/// Goes down the stack a page a frame, for ever.
#[inline(never)]
fn descend(depth: usize) -> usize {
    let mut page = [0u8; 4096];
    core::hint::black_box(&mut page);
    page[depth % 4096] = depth as u8;
    if core::hint::black_box(depth) == usize::MAX {
        return 0;
    }
    descend(depth + 1) + core::hint::black_box(page[7]) as usize
}

/// A child's: overflows its main stack into the guard below it, and catches the
/// `SIGSEGV` on its alternate stack, which ends it with 0 if the fault was in the guard.
fn child_overflow() -> i32 {
    let base = feos_user::mmap2(0, ALT_SIZE, prot::READ | prot::WRITE, map::PRIVATE);
    if base < 0 {
        return 2;
    }
    let stack = SigStack { sp: base as u64, flags: 0, reserved: 0, size: ALT_SIZE as u64 };
    if sigaltstack(Some(&stack), None) != 0 {
        return 3;
    }
    signal::sigaction(sig::SEGV, Some(&signal::action(on_overflow, sa::ONSTACK, 0)), None);
    descend(0) as i32
}

fn test_overflow(report: &mut Report) {
    let child = spawn(PROGRAM, &[PROGRAM, "--child", "overflow"]);
    let status = reap(child);
    report.check(
        "stack-overflow-handler",
        child > 0 && wait::exited(status) && wait::exit_code(status) == 0,
        format_args!("pid={child} status={status:#x}"),
    );
}

/// A thread's: after 30 ms `SIGUSR1` to the process; after 30 more, if `arg` is a
/// descriptor, a byte down it.
fn send_then_write(arg: usize) -> usize {
    sleep_through(30);
    kill(getpid(), sig::USR1);
    if arg != usize::MAX {
        sleep_through(30);
        write(arg, b"z");
    }
    0
}

/// Starts `run(arg)` on a thread that blocks `SIGUSR1`, so only this one can take it.
fn sender(run: fn(usize) -> usize, arg: usize) -> Result<thread::Thread, isize> {
    let before = mask();
    block(bit(sig::USR1));
    let started = thread::spawn(run, arg);
    sigprocmask(how::SETMASK, Some(before), None);
    started
}

fn test_interrupted_calls(report: &mut Report) {
    let Ok((reader, writer)) = pipe2(0) else {
        report.check("eintr-read", false, format_args!("no pipe"));
        return;
    };
    // No `SA_RESTART`: the read is cut short.
    reset();
    handle(sig::USR1, record, 0);
    let started = sender(send_then_write, usize::MAX);
    let start = now_ns(clock::MONOTONIC);
    let mut byte = [0u8; 1];
    let got = read(reader, &mut byte);
    let waited = (now_ns(clock::MONOTONIC) - start) / 1000;
    let _ = started.map(thread::Thread::join);
    report.check(
        "eintr-read",
        got == -errno::EINTR && HITS.load(Ordering::SeqCst) == 1 && waited < 2_000_000,
        format_args!("read={got} hits={} after={waited}us", HITS.load(Ordering::SeqCst)),
    );

    // A write that had moved some says how much: 16 KiB fit, and the rest waits.
    reset();
    let big = [7u8; 40_000];
    let started = sender(send_then_write, usize::MAX);
    let wrote = write(writer, &big);
    let _ = started.map(thread::Thread::join);
    let mut drain = [0u8; 4096];
    let mut drained = 0;
    while drained < wrote.max(0) as usize {
        let more = read(reader, &mut drain);
        if more <= 0 {
            break;
        }
        drained += more as usize;
    }
    report.check(
        "eintr-partial-write",
        wrote > 0 && (wrote as usize) < big.len() && drained == wrote as usize && HITS.load(Ordering::SeqCst) == 1,
        format_args!("write={wrote} of {} drained={drained}", big.len()),
    );

    // With `SA_RESTART` the read is made again, and gets the byte that comes after.
    reset();
    handle(sig::USR1, record, sa::RESTART);
    let started = sender(send_then_write, writer);
    let got = read(reader, &mut byte);
    let _ = started.map(thread::Thread::join);
    report.check(
        "sa-restart-read",
        got == 1 && byte[0] == b'z' && HITS.load(Ordering::SeqCst) == 1,
        format_args!("read={got} byte={:#x} hits={}", byte[0], HITS.load(Ordering::SeqCst)),
    );

    // And a `WAIT4`, for a child that ends after the signal.
    reset();
    let child = spawn(PROGRAM, &[PROGRAM, "--child", "sleep-exit"]);
    let started = sender(send_then_write, usize::MAX);
    let waited = wait4(child, 0);
    let _ = started.map(thread::Thread::join);
    set_disposition(sig::USR1, sa::DFL);
    report.check(
        "sa-restart-wait4",
        matches!(waited, Ok((pid, status)) if pid == child && wait::exit_code(status) == 7) && HITS.load(Ordering::SeqCst) == 1,
        format_args!("pid={child} wait4={waited:?} hits={}", HITS.load(Ordering::SeqCst)),
    );
    close(reader);
    close(writer);
}

static STOP: AtomicBool = AtomicBool::new(false);

/// A thread's: yields until told to stop.
fn idle_round(_: usize) -> usize {
    while !STOP.load(Ordering::SeqCst) {
        yield_now();
    }
    0
}

fn test_thread_targets(report: &mut Report) {
    // `THREAD_KILL` reaches the thread it names, from this process.
    reset();
    RAN_ON.store(0, Ordering::SeqCst);
    STOP.store(false, Ordering::SeqCst);
    handle(sig::USR1, record_thread, 0);
    let Ok(other) = thread::spawn(idle_round, 0) else {
        report.check("thread-kill-targets-thread", false, format_args!("no thread"));
        return;
    };
    let tid = other.tid;
    let sent = thread_kill(tid, sig::USR1);
    let ran = wait_until(3000, || HITS.load(Ordering::SeqCst) == 1);
    report.check(
        "thread-kill-targets-thread",
        sent == 0 && ran && RAN_ON.load(Ordering::SeqCst) == tid as u64 && CODE.load(Ordering::SeqCst) == si::TKILL && PID.load(Ordering::SeqCst) as isize == me(),
        format_args!("tid={tid} ran-on={} code={} pid={}", RAN_ON.load(Ordering::SeqCst), CODE.load(Ordering::SeqCst), PID.load(Ordering::SeqCst)),
    );

    // A process signal this thread blocks is taken by the one that doesn't.
    reset();
    RAN_ON.store(0, Ordering::SeqCst);
    block(bit(sig::USR1));
    kill(me(), sig::USR1);
    let ran = wait_until(3000, || HITS.load(Ordering::SeqCst) == 1);
    unblock(bit(sig::USR1));
    report.check(
        "process-signal-unblocked-thread",
        ran && RAN_ON.load(Ordering::SeqCst) == tid as u64 && HITS.load(Ordering::SeqCst) == 1,
        format_args!("tid={tid} ran-on={} hits={}", RAN_ON.load(Ordering::SeqCst), HITS.load(Ordering::SeqCst)),
    );
    STOP.store(true, Ordering::SeqCst);
    other.join();
    set_disposition(sig::USR1, sa::DFL);
}

fn test_process_signal_once(report: &mut Report) {
    const ROUNDS: u32 = 10_000;
    COUNT.store(0, Ordering::SeqCst);
    STOP.store(false, Ordering::SeqCst);
    handle(sig::USR1, count, 0);
    let mut threads: [Option<thread::Thread>; 8] = [const { None }; 8];
    for slot in threads.iter_mut() {
        *slot = thread::spawn(idle_round, 0).ok();
    }
    block(bit(sig::USR1));
    let start = now_ns(clock::MONOTONIC);
    let mut stuck = None;
    for round in 0..ROUNDS {
        kill(me(), sig::USR1);
        let mut spins = 0u64;
        while COUNT.load(Ordering::SeqCst) < round + 1 {
            yield_now();
            spins += 1;
            if spins.is_multiple_of(1024) && now_ns(clock::MONOTONIC) - start > 120_000_000_000 {
                stuck = Some(round);
                break;
            }
        }
        if stuck.is_some() {
            break;
        }
    }
    sleep_through(20);
    let handled = COUNT.load(Ordering::SeqCst);
    let took = (now_ns(clock::MONOTONIC) - start) / 1_000_000;
    STOP.store(true, Ordering::SeqCst);
    let made = threads.iter().filter(|thread| thread.is_some()).count();
    for thread in threads.iter_mut() {
        if let Some(thread) = thread.take() {
            thread.join();
        }
    }
    unblock(bit(sig::USR1));
    set_disposition(sig::USR1, sa::DFL);
    report.check(
        "process-signal-once",
        made == 8 && stuck.is_none() && handled == ROUNDS,
        format_args!("threads={made} sent={ROUNDS} handled={handled} stuck={stuck:?} in {took}ms"),
    );
}

static SPIN_CPU: AtomicI32 = AtomicI32::new(-1);
static SPINNING: AtomicBool = AtomicBool::new(false);

/// A thread's: spins at its program, with no call for a millisecond or so at a time,
/// saying which CPU it's on between.
fn spinner(_: usize) -> usize {
    while !STOP.load(Ordering::SeqCst) {
        SPIN_CPU.store(getcpu() as i32, Ordering::SeqCst);
        SPINNING.store(true, Ordering::SeqCst);
        for _ in 0..200_000 {
            core::hint::spin_loop();
        }
        SPINNING.store(false, Ordering::SeqCst);
    }
    0
}

fn test_ipi(report: &mut Report) {
    let cpus = feos_user::sysinfo().map_or(1, |info| info.cpus_online);
    if cpus < 2 {
        report.check("ipi-prompt-delivery", true, format_args!("skipped: one CPU"));
        return;
    }
    STOP.store(false, Ordering::SeqCst);
    handle(sig::USR1, stamp, 0);
    let Ok(other) = thread::spawn(spinner, 0) else {
        report.check("ipi-prompt-delivery", false, format_args!("no thread"));
        return;
    };
    let mut best = u64::MAX;
    let mut tries = 0;
    let deadline = now_ns(clock::MONOTONIC) + 10_000_000_000;
    while tries < 5 && now_ns(clock::MONOTONIC) < deadline {
        // Only while it spins on another CPU, so that only an IPI reaches it promptly.
        if !SPINNING.load(Ordering::SeqCst) || SPIN_CPU.load(Ordering::SeqCst) == getcpu() as i32 {
            yield_now();
            continue;
        }
        HANDLED_AT.store(0, Ordering::SeqCst);
        let sent = now_ns(clock::MONOTONIC);
        thread_kill(other.tid, sig::USR1);
        while HANDLED_AT.load(Ordering::SeqCst) == 0 && now_ns(clock::MONOTONIC) - sent < 1_000_000_000 {
            core::hint::spin_loop();
        }
        let handled = HANDLED_AT.load(Ordering::SeqCst);
        if handled != 0 {
            best = best.min(handled.saturating_sub(sent));
        }
        tries += 1;
    }
    STOP.store(true, Ordering::SeqCst);
    other.join();
    set_disposition(sig::USR1, sa::DFL);
    report.check("ipi-prompt-delivery", tries > 0 && best < 1_000_000, format_args!("cpus={cpus} tries={tries} best={}us", best / 1000));
}

/// Spawns this program as `mode` with `flags`, `sigmask` and `sigdefault`, and reaps it.
fn spawn2_child(mode: &str, flags: u64, sigmask: u64, sigdefault: u64) -> (isize, i32) {
    let mut argv = [feos_user::abi::UserStr::default(); 3];
    let argv = feos_user::user_strs(&[PROGRAM, "--child", mode], &mut argv);
    let mut request = feos_user::spawn_request(PROGRAM, argv, &[], &[]);
    request.flags = flags;
    request.sigmask = sigmask;
    request.sigdefault = sigdefault;
    let child = feos_user::spawn2(&request);
    (child, if child > 0 { reap(child) } else { -1 })
}

fn test_spawn2_signals(report: &mut Report) {
    // `SETSIGMASK` is the child's mask, less what can't be blocked; without it, the
    // spawner's.
    let (child, status) = spawn2_child("report-mask", spawn_flags::SETSIGMASK, bit(sig::USR2) | bit(sig::KILL), 0);
    block(bit(sig::HUP));
    let (inherit, inherited) = spawn2_child("report-inherited", 0, 0, 0);
    unblock(bit(sig::HUP));
    report.check(
        "spawn2-sigmask",
        child > 0 && status == 0 && inherit > 0 && inherited == 0,
        format_args!("setsigmask pid={child} status={status:#x}; inherited pid={inherit} status={inherited:#x}"),
    );
    // `SETSIGDEF` puts back what it names of what the spawner ignores, and only that.
    set_disposition(sig::USR1, sa::IGN);
    set_disposition(sig::USR2, sa::IGN);
    let (child, status) = spawn2_child("report-dispositions", spawn_flags::SETSIGDEF, 0, bit(sig::USR1));
    set_disposition(sig::USR1, sa::DFL);
    set_disposition(sig::USR2, sa::DFL);
    report.check("spawn2-setsigdef", child > 0 && status == 0, format_args!("pid={child} status={status:#x}"));
}

fn test_itimer(report: &mut Report) {
    // A blocked read cut short by `SIGALRM` 50 ms on.
    reset();
    handle(sig::ALRM, record, 0);
    let Ok((reader, writer)) = pipe2(0) else {
        report.check("alarm-interrupts-read", false, format_args!("no pipe"));
        return;
    };
    let start = now_ns(clock::MONOTONIC);
    let armed = setitimer(Some(&timer_us(50_000, 0)), None);
    let mut byte = [0u8; 1];
    let got = read(reader, &mut byte);
    let waited = (now_ns(clock::MONOTONIC) - start) / 1000;
    let after = getitimer();
    report.check(
        "alarm-interrupts-read",
        armed == 0 && got == -errno::EINTR && SIGNO.load(Ordering::SeqCst) == sig::ALRM as i32 && (40_000..1_000_000).contains(&waited)
            && after.is_ok_and(|timer| timer.value == feos_user::abi::Timeval::default()),
        format_args!("setitimer={armed} read={got} signo={} after={waited}us then {after:?}", SIGNO.load(Ordering::SeqCst)),
    );
    close(reader);
    close(writer);

    // Every 20 ms for 210 ms, then disarmed.
    COUNT.store(0, Ordering::SeqCst);
    handle(sig::ALRM, count, sa::RESTART);
    let armed = setitimer(Some(&timer_us(20_000, 20_000)), None);
    sleep_through(210);
    let disarmed = setitimer(Some(&timer_us(0, 0)), None);
    let fired = COUNT.load(Ordering::SeqCst);
    sleep_through(60);
    let later = COUNT.load(Ordering::SeqCst);
    report.check(
        "setitimer-interval",
        armed == 0 && disarmed == 0 && (5..=12).contains(&fired) && later == fired,
        format_args!("fired {fired} times in 210ms, then {later}"),
    );

    // What it's set to reads back, and so does what a new setting replaced.
    let mut replaced = feos_user::abi::ITimerVal::default();
    let set = setitimer(Some(&timer_us(1_000_000, 500_000)), Some(&mut replaced));
    let read_back = getitimer();
    let mut last = feos_user::abi::ITimerVal::default();
    let cleared = setitimer(Some(&timer_us(0, 0)), Some(&mut last));
    let gone = getitimer();
    set_disposition(sig::ALRM, sa::DFL);
    let half = feos_user::abi::Timeval { sec: 0, usec: 500_000 };
    let good_read = read_back.is_ok_and(|timer| {
        timer.interval == half && (timer.value.sec == 0 && timer.value.usec > 0 || timer.value == feos_user::abi::Timeval { sec: 1, usec: 0 })
    });
    report.check(
        "getitimer-reads",
        set == 0
            && replaced.value == feos_user::abi::Timeval::default()
            && good_read
            && cleared == 0
            && last.interval == half
            && last.value.usec + last.value.sec > 0
            && gone.is_ok_and(|timer| timer.value == feos_user::abi::Timeval::default()),
        format_args!("replaced={replaced:?} read={read_back:?} last={last:?} gone={gone:?}"),
    );
}

/// A child's: 100 ms, then exit 7.
fn child_sleep_exit() -> i32 {
    sleep_through(100);
    7
}

fn main(args: Args) -> i32 {
    match args.value_of("--child") {
        Some("sigpipe-default") => return child_sigpipe(),
        Some("spin") => return child_spin(),
        Some("call-return") => return child_call_return(),
        Some("reap-check") => return child_reap_check(),
        Some("exec-ignoring") => return child_exec_ignoring(),
        Some("overflow") => return child_overflow(),
        Some("sleep-exit") => return child_sleep_exit(),
        Some("report-mask") => return i32::from(mask() != bit(sig::USR2)),
        Some("report-inherited") => return i32::from(mask() != bit(sig::HUP)),
        Some("report-dispositions") => {
            let usr1 = disposition(sig::USR1).map(|action| action.handler);
            let usr2 = disposition(sig::USR2).map(|action| action.handler);
            return i32::from(usr1 != Ok(sa::DFL) || usr2 != Ok(sa::IGN));
        }
        Some(_) => return 2,
        None => {}
    }
    let mut report = Report { failed: 0 };
    test_handler_runs(&mut report);
    test_ucontext(&mut report);
    test_registers_survive(&mut report);
    test_flags(&mut report);
    test_faults(&mut report);
    test_sigpipe(&mut report);
    test_sigchld(&mut report);
    test_call_return(&mut report);
    test_masks(&mut report);
    test_sigpending(&mut report);
    test_sigsuspend(&mut report);
    test_altstack(&mut report);
    test_overflow(&mut report);
    test_interrupted_calls(&mut report);
    test_thread_targets(&mut report);
    test_process_signal_once(&mut report);
    test_ipi(&mut report);
    test_spawn2_signals(&mut report);
    test_itimer(&mut report);
    if report.failed == 0 {
        println!("sigtest: all ok");
        0
    } else {
        println!("sigtest: {} FAILED", report.failed);
        1
    }
}

entry!(main);
