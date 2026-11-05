//! Burns CPU with a pattern in every floating point register, across preemptions and
//! migrations, and checks it comes back: what's in a program's registers is its own.
//!     spin <seed> <milliseconds>
//! Also a thing for a kernel test to kill: `spin <seed> 0` never ends.

#![no_std]
#![no_main]

use feos_user::{entry, getcpu, parse_number, println, uptime_ms, Args};

const REGISTERS: usize = if cfg!(target_arch = "aarch64") { 32 } else { 16 };

#[repr(align(16))]
struct Bank([u128; REGISTERS]);

/// Loads the bank into the registers, spins with them untouched, and stores them.
#[cfg(target_arch = "aarch64")]
fn cycle(input: &Bank, output: &mut Bank, count: u64) {
    // SAFETY: only touches the registers listed and the two buffers given.
    unsafe {
        core::arch::asm!(
            "ldp q0, q1, [{i}, #0]", "ldp q2, q3, [{i}, #32]", "ldp q4, q5, [{i}, #64]",
            "ldp q6, q7, [{i}, #96]", "ldp q8, q9, [{i}, #128]", "ldp q10, q11, [{i}, #160]",
            "ldp q12, q13, [{i}, #192]", "ldp q14, q15, [{i}, #224]", "ldp q16, q17, [{i}, #256]",
            "ldp q18, q19, [{i}, #288]", "ldp q20, q21, [{i}, #320]", "ldp q22, q23, [{i}, #352]",
            "ldp q24, q25, [{i}, #384]", "ldp q26, q27, [{i}, #416]", "ldp q28, q29, [{i}, #448]",
            "ldp q30, q31, [{i}, #480]",
            "2:", "subs {c}, {c}, #1", "b.ne 2b",
            "stp q0, q1, [{o}, #0]", "stp q2, q3, [{o}, #32]", "stp q4, q5, [{o}, #64]",
            "stp q6, q7, [{o}, #96]", "stp q8, q9, [{o}, #128]", "stp q10, q11, [{o}, #160]",
            "stp q12, q13, [{o}, #192]", "stp q14, q15, [{o}, #224]", "stp q16, q17, [{o}, #256]",
            "stp q18, q19, [{o}, #288]", "stp q20, q21, [{o}, #320]", "stp q22, q23, [{o}, #352]",
            "stp q24, q25, [{o}, #384]", "stp q26, q27, [{o}, #416]", "stp q28, q29, [{o}, #448]",
            "stp q30, q31, [{o}, #480]",
            i = in(reg) input.0.as_ptr(),
            o = in(reg) output.0.as_mut_ptr(),
            c = inout(reg) count => _,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _,
            out("v6") _, out("v7") _, out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            out("v12") _, out("v13") _, out("v14") _, out("v15") _, out("v16") _, out("v17") _,
            out("v18") _, out("v19") _, out("v20") _, out("v21") _, out("v22") _, out("v23") _,
            out("v24") _, out("v25") _, out("v26") _, out("v27") _, out("v28") _, out("v29") _,
            out("v30") _, out("v31") _,
            options(nostack),
        );
    }
}

/// Loads the bank into the registers, spins with them untouched, and stores them.
#[cfg(target_arch = "x86_64")]
fn cycle(input: &Bank, output: &mut Bank, count: u64) {
    // SAFETY: only touches the registers listed and the two buffers given.
    unsafe {
        core::arch::asm!(
            "movdqa xmm0, [{i} + 0]", "movdqa xmm1, [{i} + 16]", "movdqa xmm2, [{i} + 32]",
            "movdqa xmm3, [{i} + 48]", "movdqa xmm4, [{i} + 64]", "movdqa xmm5, [{i} + 80]",
            "movdqa xmm6, [{i} + 96]", "movdqa xmm7, [{i} + 112]", "movdqa xmm8, [{i} + 128]",
            "movdqa xmm9, [{i} + 144]", "movdqa xmm10, [{i} + 160]", "movdqa xmm11, [{i} + 176]",
            "movdqa xmm12, [{i} + 192]", "movdqa xmm13, [{i} + 208]", "movdqa xmm14, [{i} + 224]",
            "movdqa xmm15, [{i} + 240]",
            "2:", "dec {c}", "jnz 2b",
            "movdqa [{o} + 0], xmm0", "movdqa [{o} + 16], xmm1", "movdqa [{o} + 32], xmm2",
            "movdqa [{o} + 48], xmm3", "movdqa [{o} + 64], xmm4", "movdqa [{o} + 80], xmm5",
            "movdqa [{o} + 96], xmm6", "movdqa [{o} + 112], xmm7", "movdqa [{o} + 128], xmm8",
            "movdqa [{o} + 144], xmm9", "movdqa [{o} + 160], xmm10", "movdqa [{o} + 176], xmm11",
            "movdqa [{o} + 192], xmm12", "movdqa [{o} + 208], xmm13", "movdqa [{o} + 224], xmm14",
            "movdqa [{o} + 240], xmm15",
            i = in(reg) input.0.as_ptr(),
            o = in(reg) output.0.as_mut_ptr(),
            c = inout(reg) count => _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _, out("xmm4") _,
            out("xmm5") _, out("xmm6") _, out("xmm7") _, out("xmm8") _, out("xmm9") _,
            out("xmm10") _, out("xmm11") _, out("xmm12") _, out("xmm13") _, out("xmm14") _,
            out("xmm15") _,
            options(nostack),
        );
    }
}

/// Sets this process's rounding mode to one of four, and reads back what it holds.
#[cfg(target_arch = "aarch64")]
fn set_rounding(mode: u64) {
    // SAFETY: FPCR is this process's, and this only changes its rounding.
    unsafe { core::arch::asm!("msr fpcr, {}", in(reg) (mode & 3) << 22, options(nomem, nostack)) };
}

#[cfg(target_arch = "aarch64")]
fn rounding() -> u64 {
    let value: u64;
    // SAFETY: reading FPCR has no side effects.
    unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) value, options(nomem, nostack)) };
    (value >> 22) & 3
}

#[cfg(target_arch = "x86_64")]
fn set_rounding(mode: u64) {
    let value: u32 = 0x1f80 | ((mode as u32 & 3) << 13);
    // SAFETY: MXCSR is this process's, and this only changes its rounding.
    unsafe { core::arch::asm!("ldmxcsr [{}]", in(reg) &value, options(nostack)) };
}

#[cfg(target_arch = "x86_64")]
fn rounding() -> u64 {
    let mut value: u32 = 0;
    // SAFETY: storing MXCSR has no other effect.
    unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut value, options(nostack)) };
    ((value >> 13) & 3) as u64
}

fn main(args: Args) -> i32 {
    let seed = args.get(1).and_then(parse_number).unwrap_or(1) as u128;
    let ms = args.get(2).and_then(parse_number).unwrap_or(1000);
    let forever = ms == 0;
    let start = uptime_ms() as usize;

    let mut input = Bank([0; REGISTERS]);
    for (index, value) in input.0.iter_mut().enumerate() {
        *value = (seed << 96) | (seed << 32) | ((index as u128 + 1) << 8) | 0x5a;
    }
    let mode = seed as u64 & 3;
    set_rounding(mode);

    let mut output = Bank([0; REGISTERS]);
    let mut rounds = 0u64;
    let mut cpus = 0u64;
    loop {
        cycle(&input, &mut output, 400_000);
        rounds += 1;
        if output.0 != input.0 || rounding() != mode {
            println!("spin seed={seed} rounds={rounds} CORRUPTED");
            return 3;
        }
        cpus |= 1 << getcpu();
        if !forever && uptime_ms() as usize - start >= ms {
            break;
        }
    }
    println!("spin seed={seed} rounds={rounds} cpus={cpus:#x} intact=true");
    0
}

entry!(main);
