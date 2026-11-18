//! Ordering between the CPU and bus-mastering devices.
//!
//! A CPU-only fence isn't enough once RAM is cacheable: the device is not in
//! the CPU's inner-shareable domain. Use [`wmb`] between filling a descriptor
//! and publishing it to a device, and [`rmb`] between seeing a device's index
//! change and reading what it wrote.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// The kernel's translation from its addresses to bus addresses, or zero for none.
static TRANSLATION: AtomicUsize = AtomicUsize::new(0);

/// Installs how kernel addresses that aren't identity mapped are turned into bus
/// addresses: `translate` returns `None` for an address that is its own bus address.
/// The kernel's task stacks are in a window of their own, away from the RAM behind them,
/// and a driver may be handed a buffer on one; each stack's frames are contiguous, so a
/// buffer on it is contiguous on the bus too. Call once during boot.
pub fn set_translation(translate: fn(usize) -> Option<u64>) {
    TRANSLATION.store(translate as usize, Ordering::Release);
}

/// The bus address to give a device for the kernel's `address`: the address itself, the
/// identity map's, unless it's in a range the kernel maps elsewhere. Every address a
/// driver hands a device goes through here, since a caller's buffer may be on a stack.
#[inline]
pub fn bus_address(address: usize) -> u64 {
    let hook = TRANSLATION.load(Ordering::Acquire);
    if hook == 0 {
        return address as u64;
    }
    // SAFETY: only `set_translation` stores this, from a valid function pointer.
    let translate: fn(usize) -> Option<u64> = unsafe { core::mem::transmute(hook) };
    translate(address).unwrap_or(address as u64)
}

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

/// A value a device reads or writes directly.
///
/// Shared with hardware behind the compiler's back, so it's in an [`UnsafeCell`]
/// and only reached through volatile accesses, and its address is what's handed to
/// the device. Every board identity-maps RAM, so [`phys`](Self::phys) is the address,
/// unless the buffer is somewhere the kernel maps elsewhere (a task's stack): see
/// [`bus_address`].
#[repr(transparent)]
pub struct DmaBuf<T> {
    value: UnsafeCell<T>,
}

// SAFETY: sharing this only shares its address. Every access is by a volatile
// operation, or through `unsafe` code that must order itself against the device.
unsafe impl<T: Send> Sync for DmaBuf<T> {}

impl<T> DmaBuf<T> {
    /// Wraps a value for a device to use.
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
        }
    }

    /// The bus address to give the device.
    pub fn phys(&self) -> u64 {
        bus_address(self.value.get() as usize)
    }

    /// A raw pointer to the value, for accessing part of it.
    pub const fn as_ptr(&self) -> *mut T {
        self.value.get()
    }
}

impl<T: Copy> DmaBuf<T> {
    /// Reads the value. Use [`rmb`] first if the device has signalled it through
    /// something else.
    pub fn read(&self) -> T {
        // SAFETY: the cell holds a valid `T`, and `T: Copy` means no ownership moves.
        unsafe { core::ptr::read_volatile(self.value.get()) }
    }

    /// Writes the value. Use [`wmb`] before telling the device.
    pub fn write(&self, value: T) {
        // SAFETY: the cell holds a `T`, which `T: Copy` means needs no dropping.
        unsafe { core::ptr::write_volatile(self.value.get(), value) };
    }
}

#[cfg(test)]
mod dma_buf_tests {
    use super::*;

    #[test]
    fn reads_what_was_written_at_the_address_it_reports() {
        let buffer = DmaBuf::new(7u32);
        assert_eq!(buffer.read(), 7);
        buffer.write(9);
        assert_eq!(buffer.read(), 9);
        assert_eq!(buffer.phys(), buffer.as_ptr() as u64);
        // SAFETY: nothing else is using it.
        assert_eq!(unsafe { *(buffer.phys() as usize as *const u32) }, 9);
    }

    #[test]
    fn it_is_no_bigger_than_the_value() {
        assert_eq!(core::mem::size_of::<DmaBuf<u64>>(), 8);
        assert_eq!(core::mem::align_of::<DmaBuf<u64>>(), 8);
    }
}
