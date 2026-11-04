//! Ordering between the CPU and bus-mastering devices.
//!
//! A CPU-only fence isn't enough once RAM is cacheable: the device is not in
//! the CPU's inner-shareable domain. Use [`wmb`] between filling a descriptor
//! and publishing it to a device, and [`rmb`] between seeing a device's index
//! change and reading what it wrote.

/// Orders earlier writes to DMA memory before later writes.
#[inline]
pub fn wmb() {
    imp::wmb();
}

/// Orders earlier reads of DMA memory before later reads.
#[inline]
pub fn rmb() {
    imp::rmb();
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod imp {
    use core::arch::asm;

    #[inline]
    pub fn wmb() {
        // SAFETY: a barrier has no effect other than ordering.
        unsafe { asm!("dmb oshst", options(nostack, preserves_flags)) };
    }

    #[inline]
    pub fn rmb() {
        // SAFETY: a barrier has no effect other than ordering.
        unsafe { asm!("dmb oshld", options(nostack, preserves_flags)) };
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
mod imp {
    use core::sync::atomic::{fence, Ordering};

    // x86 keeps write-back memory coherent with bus masters and orders ordinary
    // stores and loads, so only the compiler needs restraining; `fence` does
    // that and also serves host tests.
    #[inline]
    pub fn wmb() {
        fence(Ordering::SeqCst);
    }

    #[inline]
    pub fn rmb() {
        fence(Ordering::SeqCst);
    }
}
