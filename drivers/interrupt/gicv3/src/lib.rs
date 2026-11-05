//! ARM Generic Interrupt Controller v3 (GICv3) driver.
//!
//! The board finds the distributor and redistributors in the device tree and calls
//! [`GicV3::init`]; the kernel then uses the HAL [`InterruptController`] trait. Unlike
//! a GICv2, a CPU reaches its interface through system registers and has a
//! redistributor of its own, for the SGIs and PPIs that are private to it.
//!
//! QEMU's `virt` machine has no EL3, so the GIC has one security state: Group 1
//! interrupts, the only kind used here, go to EL1 as IRQs. It's put in affinity
//! routing mode, as a GICv3 needs for more than eight CPUs. Message-signalled
//! interrupts need an ITS, which this driver doesn't have.

#![cfg_attr(not(test), no_std)]

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

use feos_hal::{InterruptController, IpiTarget, IrqTrigger};

/// Distributor registers.
const GICD_CTLR: usize = 0x0000;
const GICD_TYPER: usize = 0x0004;
const GICD_IGROUPR: usize = 0x0080;
const GICD_ISENABLER: usize = 0x0100;
const GICD_ICENABLER: usize = 0x0180;
const GICD_IPRIORITYR: usize = 0x0400;
const GICD_ICFGR: usize = 0x0c00;
const GICD_IROUTER: usize = 0x6000;

/// `GICD_CTLR` with one security state: Group 1 on, affinity routing on.
const CTLR_ENABLE_GROUP1: u32 = 1 << 1;
const CTLR_ARE: u32 = 1 << 4;
/// `GICD_CTLR`: a write hasn't finished.
const CTLR_RWP: u32 = 1 << 31;

/// Redistributor registers, in its first 64 KiB frame.
const GICR_CTLR: usize = 0x0000;
const GICR_TYPER: usize = 0x0008;
const GICR_WAKER: usize = 0x0014;
/// `GICR_WAKER`: this CPU is asleep as far as the GIC's concerned.
const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
/// `GICR_WAKER`: its interface has gone quiet.
const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
/// `GICR_TYPER`: this is the last redistributor in the region.
const TYPER_LAST: u64 = 1 << 4;
/// Each redistributor takes two 64 KiB frames, without VLPI support.
const GICR_STRIDE: usize = 0x2_0000;
/// The second frame, holding what a GICv2 has in the distributor's banked registers.
const GICR_SGI_FRAME: usize = 0x1_0000;
/// `GICR_CTLR`: a write hasn't finished.
const GICR_RWP: u32 = 1 << 3;

/// Priority assigned to every interrupt the kernel enables.
const PRIORITY: u32 = 0x80;
/// How many times a "still writing" bit is polled. A stuck GIC shouldn't hang boot.
const POLLS: usize = 100_000;

/// SGIs and PPIs are private to a CPU, and SPIs start here.
const FIRST_SPI: u32 = 32;

/// Interrupt ID reported when no real interrupt is pending.
pub const SPURIOUS: u32 = 1023;

/// The `ICC_SGI1R_EL1` value that sends SGI `id`, by affinity. The boards' CPUs are
/// all in one cluster, with affinity 0 as their logical index.
fn sgi_value(target: IpiTarget, id: u32) -> u64 {
    let id = u64::from(id & 0xf) << 24;
    match target {
        IpiTarget::Cpu(cpu) => id | (1u64 << (cpu as u32 & 15)),
        // IRM: every CPU but this one.
        IpiTarget::AllOthers => id | (1u64 << 40),
    }
}

/// A GICv3 distributor and its redistributors.
pub struct GicV3 {
    distributor: AtomicUsize,
    redistributors: AtomicUsize,
}

impl GicV3 {
    /// A driver with no addresses yet: see [`set_bases`](Self::set_bases).
    pub const fn new() -> Self {
        Self {
            distributor: AtomicUsize::new(0),
            redistributors: AtomicUsize::new(0),
        }
    }

    /// Gives the driver the addresses the device tree names.
    pub fn set_bases(&self, distributor: usize, redistributors: usize) {
        self.distributor.store(distributor, Ordering::Relaxed);
        self.redistributors.store(redistributors, Ordering::Relaxed);
    }

    fn dist(&self) -> usize {
        self.distributor.load(Ordering::Relaxed)
    }

    fn wait_distributor(&self) {
        for _ in 0..POLLS {
            if read32(self.dist() + GICD_CTLR) & CTLR_RWP == 0 {
                return;
            }
            core::hint::spin_loop();
        }
    }

    /// Finds this CPU's redistributor, by the affinity it reports.
    fn own_redistributor(&self) -> Option<usize> {
        let mut base = self.redistributors.load(Ordering::Relaxed);
        if base == 0 {
            return None;
        }
        let affinity = sys::affinity();
        // A bound in case the "last" bit never comes.
        for _ in 0..256 {
            let typer = read64(base + GICR_TYPER);
            if (typer >> 32) as u32 == affinity {
                return Some(base);
            }
            if typer & TYPER_LAST != 0 {
                return None;
            }
            base += GICR_STRIDE;
        }
        None
    }

    /// Configures the distributor and the boot CPU. Every interrupt starts disabled,
    /// in Group 1.
    pub fn init(&self) {
        let dist = self.dist();
        write32(dist + GICD_CTLR, 0);
        self.wait_distributor();

        let typer = read32(dist + GICD_TYPER);
        let lines = ((typer & 0x1f) + 1) * 32;
        let mut irq = FIRST_SPI;
        while irq < lines {
            write32(dist + GICD_ICENABLER + (irq as usize / 32) * 4, u32::MAX);
            write32(dist + GICD_IGROUPR + (irq as usize / 32) * 4, u32::MAX);
            irq += 32;
        }
        self.wait_distributor();

        write32(dist + GICD_CTLR, CTLR_ARE | CTLR_ENABLE_GROUP1);
        self.wait_distributor();

        self.init_cpu();
    }

    /// Configures the calling CPU's redistributor and interface.
    pub fn init_cpu(&self) {
        if let Some(redistributor) = self.own_redistributor() {
            // Wake it: a redistributor comes up thinking its CPU is asleep.
            let waker = read32(redistributor + GICR_WAKER);
            write32(redistributor + GICR_WAKER, waker & !WAKER_PROCESSOR_SLEEP);
            for _ in 0..POLLS {
                if read32(redistributor + GICR_WAKER) & WAKER_CHILDREN_ASLEEP == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
            let sgi = redistributor + GICR_SGI_FRAME;
            write32(sgi + GICD_ICENABLER, u32::MAX);
            write32(sgi + GICD_IGROUPR, u32::MAX);
            for _ in 0..POLLS {
                if read32(redistributor + GICR_CTLR) & GICR_RWP == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }
        sys::init_interface();
    }

    /// The registers that control `irq`: the CPU's redistributor for one private to
    /// it, and the distributor for the rest.
    fn frame(&self, irq: u32) -> Option<usize> {
        if irq < FIRST_SPI {
            self.own_redistributor().map(|base| base + GICR_SGI_FRAME)
        } else {
            Some(self.dist())
        }
    }
}

impl Default for GicV3 {
    fn default() -> Self {
        Self::new()
    }
}

impl InterruptController for GicV3 {
    fn enable(&self, irq: u32) {
        if irq >= SPURIOUS {
            return;
        }
        let Some(frame) = self.frame(irq) else {
            return;
        };
        let register = frame + GICD_IPRIORITYR + (irq as usize & !3);
        let shift = (irq % 4) * 8;
        let value = read32(register) & !(0xffu32 << shift);
        write32(register, value | PRIORITY << shift);
        write32(frame + GICD_ISENABLER + (irq as usize / 32) * 4, 1u32 << (irq % 32));
        sys::barrier();
    }

    fn disable(&self, irq: u32) {
        if irq >= SPURIOUS {
            return;
        }
        if let Some(frame) = self.frame(irq) {
            write32(frame + GICD_ICENABLER + (irq as usize / 32) * 4, 1u32 << (irq % 32));
            sys::barrier();
        }
    }

    fn configure(&self, irq: u32, trigger: IrqTrigger, cpu: usize) {
        if !(FIRST_SPI..SPURIOUS).contains(&irq) {
            return;
        }
        let dist = self.dist();
        let register = dist + GICD_ICFGR + (irq as usize / 16) * 4;
        let bit = 1u32 << ((irq % 16) * 2 + 1);
        let value = read32(register);
        let edge = matches!(trigger, IrqTrigger::Edge);
        write32(register, if edge { value | bit } else { value & !bit });
        write64(dist + GICD_IROUTER + irq as usize * 8, cpu as u64);
        sys::barrier();
    }

    fn acknowledge(&self) -> u32 {
        sys::acknowledge() & 0xff_ffff
    }

    fn end(&self, irq: u32) {
        if irq < 1020 {
            sys::end(irq);
        }
    }

    fn is_spurious(&self, id: u32) -> bool {
        (1020..1024).contains(&id)
    }

    fn send_ipi(&self, target: IpiTarget, id: u32) {
        sys::send_sgi(sgi_value(target, id));
    }
}

#[inline]
fn read32(address: usize) -> u32 {
    // SAFETY: callers pass addresses inside the GIC windows the board gave.
    unsafe { read_volatile(address as *const u32) }
}

#[inline]
fn write32(address: usize, value: u32) {
    // SAFETY: as in `read32`.
    unsafe { write_volatile(address as *mut u32, value) }
}

#[inline]
fn read64(address: usize) -> u64 {
    // SAFETY: as in `read32`.
    unsafe { read_volatile(address as *const u64) }
}

#[inline]
fn write64(address: usize, value: u64) {
    // SAFETY: as in `read32`.
    unsafe { write_volatile(address as *mut u64, value) }
}

/// The CPU interface, in system registers.
#[cfg(target_arch = "aarch64")]
mod sys {
    use core::arch::asm;

    /// This CPU's affinity, in the form `GICR_TYPER` reports it: Aff3 to Aff0.
    pub fn affinity() -> u32 {
        let mpidr: u64;
        // SAFETY: reading a system register has no side effects.
        unsafe { asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
        (mpidr & 0xff_ffff) as u32 | ((mpidr >> 32) as u32 & 0xff) << 24
    }

    pub fn init_interface() {
        // SAFETY: these configure this CPU's interface, and nothing else.
        unsafe {
            // Use the system registers rather than a memory-mapped interface.
            asm!(
                "mrs {scratch}, S3_0_C12_C12_5",
                "orr {scratch}, {scratch}, #1",
                "msr S3_0_C12_C12_5, {scratch}",
                "isb",
                scratch = out(reg) _,
                options(nostack),
            );
            // ICC_PMR_EL1: all priorities. ICC_BPR1_EL1: no preemption grouping.
            asm!("msr S3_0_C4_C6_0, {}", in(reg) 0xffu64, options(nostack));
            asm!("msr S3_0_C12_C12_3, {}", in(reg) 0u64, options(nostack));
            // ICC_IGRPEN1_EL1: Group 1 on.
            asm!("msr S3_0_C12_C12_7, {}", in(reg) 1u64, options(nostack));
            asm!("isb", options(nostack));
        }
    }

    pub fn acknowledge() -> u32 {
        let value: u64;
        // SAFETY: reading ICC_IAR1_EL1 acknowledges the interrupt, which is the point.
        unsafe { asm!("mrs {}, S3_0_C12_C12_0", out(reg) value, options(nostack)) };
        value as u32
    }

    pub fn end(irq: u32) {
        // SAFETY: ICC_EOIR1_EL1 takes the ID that was acknowledged.
        unsafe { asm!("msr S3_0_C12_C12_1, {}", in(reg) u64::from(irq), options(nostack)) };
    }

    pub fn send_sgi(value: u64) {
        // SAFETY: writing ICC_SGI1R_EL1 sends the SGI, which is the point.
        unsafe {
            asm!("dsb ishst", "msr S3_0_C12_C11_5, {}", "isb", in(reg) value, options(nostack));
        }
    }

    pub fn barrier() {
        // SAFETY: a barrier has no effect other than ordering.
        unsafe { asm!("dsb sy", "isb", options(nostack)) };
    }
}

/// Stand-ins, so the crate's pure parts build and test on the host.
#[cfg(not(target_arch = "aarch64"))]
mod sys {
    pub fn affinity() -> u32 {
        0
    }
    pub fn init_interface() {}
    pub fn acknowledge() -> u32 {
        super::SPURIOUS
    }
    pub fn end(_irq: u32) {}
    pub fn send_sgi(_value: u64) {}
    pub fn barrier() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgis_are_addressed_by_affinity() {
        assert_eq!(sgi_value(IpiTarget::Cpu(0), 1), 1 << 24 | 1);
        assert_eq!(sgi_value(IpiTarget::Cpu(3), 0), 1 << 3);
        assert_eq!(sgi_value(IpiTarget::AllOthers, 2), 2 << 24 | 1 << 40);
    }

    #[test]
    fn only_special_ids_are_spurious() {
        let gic = GicV3::new();
        assert!(gic.is_spurious(1023));
        assert!(gic.is_spurious(1020));
        assert!(!gic.is_spurious(30));
        assert!(!gic.is_spurious(1019));
    }
}
