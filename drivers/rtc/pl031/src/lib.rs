//! The ARM PL031 real-time clock, as on QEMU's `virt` machine.
//!
//! Its data register counts seconds, and QEMU starts it at the host's time. Reset
//! leaves it counting from zero on hardware with no battery, so a value before
//! any plausible boot date is reported as no time at all.

#![no_std]

use core::ptr::read_volatile;

use feos_hal::RealTimeClock;

/// Data register: the current count.
const RTCDR: usize = 0x000;
/// PrimeCell peripheral ID register 0, which reads 0x31 on a PL031.
const PERIPHERAL_ID0: usize = 0xfe0;

/// 2001-09-09: the counter has been set by something, if it's past this.
const EARLIEST_PLAUSIBLE: u32 = 1_000_000_000;

/// A PL031 at a board-supplied address.
#[derive(Clone, Copy, Debug)]
pub struct Pl031 {
    base: usize,
}

impl Pl031 {
    /// Wraps the device at `base`.
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: `base` is the device window the board mapped.
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    /// True when the ID register says a PL031 is at the address.
    pub fn is_present(&self) -> bool {
        self.read(PERIPHERAL_ID0) & 0xff == 0x31
    }
}

/// What a raw count means. Separate so that it can be tested.
const fn seconds_from_count(count: u32) -> Option<u64> {
    if count < EARLIEST_PLAUSIBLE {
        None
    } else {
        Some(count as u64)
    }
}

impl RealTimeClock for Pl031 {
    fn unix_time(&self) -> Option<u64> {
        if !self.is_present() {
            return None;
        }
        seconds_from_count(self.read(RTCDR))
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_that_was_never_set_means_nothing() {
        assert_eq!(seconds_from_count(12), None);
        assert_eq!(seconds_from_count(1_800_000_000), Some(1_800_000_000));
    }
}
