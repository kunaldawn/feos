//! PSCI power management calls.
//!
//! Firmware says how to reach PSCI in the device tree's `/psci` node, as a hypervisor
//! or a secure monitor call. QEMU's `virt` uses HVC unless the guest could use that
//! itself, with EL2, when it uses SMC. Machines with secure firmware have their own,
//! and in QEMU's case `secure=on` without any has none at all.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

use feos_hal::FirmwareConduit;

const CPU_ON: u64 = 0xC400_0003;
const SYSTEM_OFF: u64 = 0x8400_0008;
const SYSTEM_RESET: u64 = 0x8400_0009;
/// PSCI's "not supported".
const NOT_SUPPORTED: i64 = -1;

const HVC: u8 = 0;
const SMC: u8 = 1;
const NONE: u8 = 2;

static CONDUIT: AtomicU8 = AtomicU8::new(HVC);

/// Says how to reach firmware. Before this, it's by HVC.
pub fn set_conduit(conduit: FirmwareConduit) {
    let value = match conduit {
        FirmwareConduit::Unknown | FirmwareConduit::Hvc => HVC,
        FirmwareConduit::Smc => SMC,
        FirmwareConduit::None => NONE,
    };
    CONDUIT.store(value, Ordering::Release);
}

/// Makes a PSCI call.
///
/// SMCCC permits an HVC or SMC from EL1, and allows x4-x17 to be clobbered.
fn call(function: u64, a: u64, b: u64, c: u64) -> i64 {
    let result: u64;
    match CONDUIT.load(Ordering::Acquire) {
        // SAFETY: as above: PSCI owns the call.
        HVC => unsafe {
            asm!(
                "hvc #0",
                inout("x0") function => result,
                in("x1") a,
                in("x2") b,
                in("x3") c,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack),
            );
        },
        // SAFETY: as above.
        SMC => unsafe {
            asm!(
                "smc #0",
                inout("x0") function => result,
                in("x1") a,
                in("x2") b,
                in("x3") c,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack),
            );
        },
        _ => return NOT_SUPPORTED,
    }
    result as i64
}

/// Starts the CPU identified by `target` at `entry` with `context` in `x0`.
///
/// `target` is a PSCI affinity value in `MPIDR_EL1` format. The function
/// returns `Ok(())` once firmware accepted the request; the new CPU may not
/// have executed its first instruction yet.
///
/// Errors are the raw PSCI status codes, for example `-2`
/// (`INVALID_PARAMETERS`) when the machine has fewer CPUs than requested,
/// `-4` (`ALREADY_ON`) for an already-running CPU, or `-1` (`NOT_SUPPORTED`) when
/// there's no firmware to ask.
pub fn cpu_on(target: u64, entry: u64, context: u64) -> Result<(), i64> {
    match call(CPU_ON, target, entry, context) {
        0 => Ok(()),
        code => Err(code),
    }
}

/// Powers the machine off (QEMU exits). Without firmware, just stops.
pub fn system_off() -> ! {
    call(SYSTEM_OFF, 0, 0, 0);
    crate::cpu::halt()
}

/// Resets the machine. Without firmware, just stops.
pub fn system_reset() -> ! {
    call(SYSTEM_RESET, 0, 0, 0);
    crate::cpu::halt()
}
