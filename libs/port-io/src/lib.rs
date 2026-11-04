//! x86 port I/O.
//!
//! Every x86 driver and the architecture crate talk to legacy devices through
//! these functions, so the instruction encodings and their compiler contract
//! live in one place. None of them is `nomem`: a port write can start DMA or
//! change interrupt routing, so the compiler must not move memory accesses
//! across it.
//!
//! The crate is empty on other architectures, which lets a driver that supports
//! both transports (PCI, fw_cfg) depend on it unconditionally.

#![no_std]

#[cfg(target_arch = "x86_64")]
mod imp {
    use core::arch::asm;

    /// Reads a byte from `port`.
    ///
    /// # Safety
    /// `port` must be a port this caller may read; a read can have device side
    /// effects.
    #[inline]
    pub unsafe fn inb(port: u16) -> u8 {
        let value: u8;
        // SAFETY: the caller upholds the port contract.
        unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nostack)) };
        value
    }

    /// Reads a 32-bit value from `port`.
    ///
    /// # Safety
    /// See [`inb`].
    #[inline]
    pub unsafe fn inl(port: u16) -> u32 {
        let value: u32;
        // SAFETY: the caller upholds the port contract.
        unsafe { asm!("in eax, dx", out("eax") value, in("dx") port, options(nostack)) };
        value
    }

    /// Writes a byte to `port`.
    ///
    /// # Safety
    /// `port` must be a port this caller may write.
    #[inline]
    pub unsafe fn outb(port: u16, value: u8) {
        // SAFETY: the caller upholds the port contract.
        unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nostack)) };
    }

    /// Writes a 16-bit value to `port`.
    ///
    /// # Safety
    /// See [`outb`].
    #[inline]
    pub unsafe fn outw(port: u16, value: u16) {
        // SAFETY: the caller upholds the port contract.
        unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nostack)) };
    }

    /// Writes a 32-bit value to `port`.
    ///
    /// # Safety
    /// See [`outb`].
    #[inline]
    pub unsafe fn outl(port: u16, value: u32) {
        // SAFETY: the caller upholds the port contract.
        unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nostack)) };
    }
}

#[cfg(target_arch = "x86_64")]
pub use imp::*;
