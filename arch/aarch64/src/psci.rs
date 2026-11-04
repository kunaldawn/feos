//! PSCI power management calls.
//!
//! On the QEMU `virt` machine firmware exposes PSCI over the HVC conduit when
//! the kernel runs at EL1, which lets us power the machine off cleanly and
//! start secondary CPUs.

use core::arch::asm;

const CPU_ON: u64 = 0xC400_0003;
const SYSTEM_OFF: u64 = 0x8400_0008;
const SYSTEM_RESET: u64 = 0x8400_0009;

/// Starts the CPU identified by `target` at `entry` with `context` in `x0`.
///
/// `target` is a PSCI affinity value in `MPIDR_EL1` format. The function
/// returns `Ok(())` once firmware accepted the request; the new CPU may not
/// have executed its first instruction yet.
///
/// Errors are the raw PSCI status codes, for example `-2`
/// (`INVALID_PARAMETERS`) when the machine has fewer CPUs than requested or
/// `-4` (`ALREADY_ON`) for an already-running CPU.
pub fn cpu_on(target: u64, entry: u64, context: u64) -> Result<(), i64> {
    let result: u64;
    // SAFETY: SMCCC permits an HVC at EL1 and PSCI owns the call. The SMCCC
    // ABI allows x4-x17 to be clobbered by the callee.
    unsafe {
        asm!(
            "hvc #0",
            inout("x0") CPU_ON => result,
            in("x1") target,
            in("x2") entry,
            in("x3") context,
            lateout("x4") _,
            lateout("x5") _,
            lateout("x6") _,
            lateout("x7") _,
            lateout("x8") _,
            lateout("x9") _,
            lateout("x10") _,
            lateout("x11") _,
            lateout("x12") _,
            lateout("x13") _,
            lateout("x14") _,
            lateout("x15") _,
            lateout("x16") _,
            lateout("x17") _,
            options(nostack),
        )
    };

    match result as i64 {
        0 => Ok(()),
        code => Err(code),
    }
}

/// Powers the machine off (QEMU exits).
pub fn system_off() -> ! {
    unsafe { asm!("hvc #0", in("x0") SYSTEM_OFF, options(noreturn)) }
}

/// Resets the machine.
pub fn system_reset() -> ! {
    unsafe { asm!("hvc #0", in("x0") SYSTEM_RESET, options(noreturn)) }
}
