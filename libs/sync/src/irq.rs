//! Masking interrupts on the current CPU.
//!
//! This is the one place the crate touches the CPU. The operations are not
//! `nomem`: they act as compiler barriers, so memory accesses cannot move out
//! of the masked region. Hosted builds (host tests) have no interrupts; the
//! test build models the mask per thread so the guards can be checked.

/// Masks interrupts and returns the previous state for [`restore`].
#[inline]
pub fn save_and_disable() -> u64 {
    imp::save_and_disable()
}

/// Restores an interrupt state captured by [`save_and_disable`].
#[inline]
pub fn restore(flags: u64) {
    imp::restore(flags)
}

/// Returns `true` when interrupts are unmasked on this CPU.
#[inline]
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod imp {
    use core::arch::asm;

    /// `DAIF.I`.
    const IRQ_MASKED: u64 = 1 << 7;

    #[inline]
    pub fn save_and_disable() -> u64 {
        let flags: u64;
        // SAFETY: reading and setting DAIF is always allowed at EL1.
        unsafe { asm!("mrs {}, daif", "msr daifset, #2", out(reg) flags, options(nostack)) };
        flags
    }

    #[inline]
    pub fn restore(flags: u64) {
        // SAFETY: `flags` comes from `save_and_disable`, which read DAIF.
        unsafe { asm!("msr daif, {}", in(reg) flags, options(nostack)) };
    }

    #[inline]
    pub fn is_enabled() -> bool {
        let flags: u64;
        // SAFETY: reading DAIF has no side effects.
        unsafe { asm!("mrs {}, daif", out(reg) flags, options(nomem, nostack)) };
        flags & IRQ_MASKED == 0
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod imp {
    use core::arch::asm;

    /// `RFLAGS.IF`.
    const IRQ_ENABLED: u64 = 1 << 9;

    #[inline]
    pub fn save_and_disable() -> u64 {
        let flags: u64;
        // SAFETY: `pushfq`/`pop` balance on the stack, `cli` is ring-0 safe.
        unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags) };
        flags
    }

    #[inline]
    pub fn restore(flags: u64) {
        // SAFETY: `flags` comes from `save_and_disable`, which read RFLAGS.
        unsafe { asm!("push {}", "popfq", in(reg) flags) };
    }

    #[inline]
    pub fn is_enabled() -> bool {
        let flags: u64;
        // SAFETY: `pushfq`/`pop` balance on the stack.
        unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(nomem)) };
        flags & IRQ_ENABLED != 0
    }
}

#[cfg(all(not(target_os = "none"), test))]
mod imp {
    extern crate std;

    use std::cell::Cell;

    std::thread_local! {
        static MASKED: Cell<bool> = const { Cell::new(false) };
    }

    pub fn save_and_disable() -> u64 {
        MASKED.with(|masked| masked.replace(true)) as u64
    }

    pub fn restore(flags: u64) {
        MASKED.with(|masked| masked.set(flags != 0));
    }

    pub fn is_enabled() -> bool {
        !MASKED.with(|masked| masked.get())
    }
}

#[cfg(all(
    not(test),
    not(all(any(target_arch = "aarch64", target_arch = "x86_64"), target_os = "none"))
))]
mod imp {
    #[inline]
    pub fn save_and_disable() -> u64 {
        0
    }

    #[inline]
    pub fn restore(_flags: u64) {}

    #[inline]
    pub fn is_enabled() -> bool {
        true
    }
}
