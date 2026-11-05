//! The parts of virtio that every device shares.
//!
//! [`Transport`] hides how a device is reached: [`MmioTransport`] covers the
//! `virtio-mmio` slots of QEMU's `virt` machine (both the legacy version 1 that
//! QEMU defaults to and the modern version 2), and [`PciTransport`] covers
//! modern `virtio-pci` devices on either architecture. [`Queue`] is a split
//! virtqueue in caller-owned memory, and [`negotiate`] runs the status and
//! feature handshake.
//!
//! Queues start out asking the device not to raise interrupts, so everything can
//! be polled. A driver that has an interrupt line turns them on for a queue with
//! [`Queue::set_interrupts`], and its handler calls
//! [`Transport::acknowledge_interrupt`].

#![cfg_attr(not(test), no_std)]

mod mmio;
mod pci;
mod queue;

pub use mmio::{MmioTransport, MMIO_MAGIC};
pub use pci::PciTransport;
pub use queue::{Descriptor, Queue, Segment, DESC_F_NEXT, DESC_F_WRITE};

/// Virtio device IDs.
pub mod device {
    /// A network card.
    pub const NET: u32 = 1;
    /// A block device.
    pub const BLOCK: u32 = 2;
    /// A GPU.
    pub const GPU: u32 = 16;
    /// An input device.
    pub const INPUT: u32 = 18;
}

/// Device status bits.
pub mod status {
    /// The guest has noticed the device.
    pub const ACKNOWLEDGE: u32 = 1;
    /// The guest knows how to drive it.
    pub const DRIVER: u32 = 2;
    /// The driver is ready.
    pub const DRIVER_OK: u32 = 4;
    /// Feature negotiation has finished.
    pub const FEATURES_OK: u32 = 8;
    /// The device hit an error it can't recover from.
    pub const NEEDS_RESET: u32 = 64;
    /// The guest has given up on the device.
    pub const FAILED: u32 = 128;
}

/// Feature bit: the device follows the modern (1.0) interface.
pub const F_VERSION_1: u64 = 1 << 32;

/// How a driver reaches its device.
pub trait Transport {
    /// The virtio device ID, or zero for an empty slot.
    fn device_id(&self) -> u32;

    /// True when the device needs [`F_VERSION_1`] and `FEATURES_OK`.
    fn is_modern(&self) -> bool;

    /// Reads the status register.
    fn status(&self) -> u32;

    /// Writes the status register. Zero resets the device.
    fn set_status(&self, status: u32);

    /// The features the device offers. A legacy device has only the low half.
    fn device_features(&self) -> u64;

    /// Tells the device which features the driver accepts.
    fn set_driver_features(&self, features: u64);

    /// Largest size queue `index` can have, or zero when it doesn't exist.
    fn queue_max(&self, index: u16) -> u16;

    /// Hands a queue to the device. Returns `false` if it can't be used.
    ///
    /// The three addresses are physical. A legacy device derives the last two
    /// from `desc`, which [`Queue`]'s layout makes correct.
    fn queue_activate(&self, index: u16, size: u16, desc: u64, avail: u64, used: u64) -> bool;

    /// Tells the device that queue `index` has new buffers.
    fn queue_notify(&self, index: u16);

    /// Quiets the device's interrupt line, and says whether it was asserting it.
    /// Message-signalled interrupts don't need this. The handler for a line calls
    /// it before the controller sees the end of the interrupt.
    fn acknowledge_interrupt(&self) -> bool {
        false
    }

    /// Reads a byte of the device-specific configuration.
    fn config_read8(&self, offset: usize) -> u8;

    /// Writes a byte of the device-specific configuration.
    fn config_write8(&self, offset: usize, value: u8);

    /// Reads a little-endian word of the device-specific configuration.
    fn config_read32(&self, offset: usize) -> u32;

    /// Reads a little-endian double word as two reads, which is all a device
    /// promises. Config that can change would need the generation counter; the
    /// fields read this way are fixed for a device's lifetime.
    fn config_read64(&self, offset: usize) -> u64 {
        self.config_read32(offset) as u64 | (self.config_read32(offset + 4) as u64) << 32
    }
}

/// Either transport, for code that picks one at run time.
#[derive(Clone, Copy, Debug)]
pub enum AnyTransport {
    /// A `virtio-mmio` slot.
    Mmio(MmioTransport),
    /// A `virtio-pci` function.
    Pci(PciTransport),
}

macro_rules! either {
    ($self:ident, $inner:ident => $body:expr) => {
        match $self {
            AnyTransport::Mmio($inner) => $body,
            AnyTransport::Pci($inner) => $body,
        }
    };
}

impl Transport for AnyTransport {
    fn device_id(&self) -> u32 {
        either!(self, inner => inner.device_id())
    }
    fn is_modern(&self) -> bool {
        either!(self, inner => inner.is_modern())
    }
    fn status(&self) -> u32 {
        either!(self, inner => inner.status())
    }
    fn set_status(&self, status: u32) {
        either!(self, inner => inner.set_status(status))
    }
    fn device_features(&self) -> u64 {
        either!(self, inner => inner.device_features())
    }
    fn set_driver_features(&self, features: u64) {
        either!(self, inner => inner.set_driver_features(features))
    }
    fn queue_max(&self, index: u16) -> u16 {
        either!(self, inner => inner.queue_max(index))
    }
    fn queue_activate(&self, index: u16, size: u16, desc: u64, avail: u64, used: u64) -> bool {
        either!(self, inner => inner.queue_activate(index, size, desc, avail, used))
    }
    fn queue_notify(&self, index: u16) {
        either!(self, inner => inner.queue_notify(index))
    }
    fn acknowledge_interrupt(&self) -> bool {
        either!(self, inner => inner.acknowledge_interrupt())
    }
    fn config_read8(&self, offset: usize) -> u8 {
        either!(self, inner => inner.config_read8(offset))
    }
    fn config_write8(&self, offset: usize, value: u8) {
        either!(self, inner => inner.config_write8(offset, value))
    }
    fn config_read32(&self, offset: usize) -> u32 {
        either!(self, inner => inner.config_read32(offset))
    }
}

/// Resets the device and negotiates features.
///
/// The result is `wanted` restricted to what the device offers, plus
/// [`F_VERSION_1`] on a modern device. Returns `None` when the device refuses,
/// having told it so. Queues are set up next, and [`finish`] ends the handshake.
pub fn negotiate<T: Transport>(transport: &T, wanted: u64) -> Option<u64> {
    transport.set_status(0);
    // A reset takes effect once the status reads back as zero.
    let mut spins = 0u32;
    while transport.status() != 0 {
        spins += 1;
        if spins > 1_000_000 {
            return None;
        }
        core::hint::spin_loop();
    }
    transport.set_status(status::ACKNOWLEDGE);
    transport.set_status(status::ACKNOWLEDGE | status::DRIVER);

    let offered = transport.device_features();
    let mut accepted = offered & wanted;
    if transport.is_modern() {
        if offered & F_VERSION_1 == 0 {
            transport.set_status(status::FAILED);
            return None;
        }
        accepted |= F_VERSION_1;
    } else {
        accepted &= u32::MAX as u64;
    }
    transport.set_driver_features(accepted);

    if transport.is_modern() {
        let state = status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK;
        transport.set_status(state);
        if transport.status() & status::FEATURES_OK == 0 {
            transport.set_status(status::FAILED);
            return None;
        }
    }
    Some(accepted)
}

/// Ends the handshake: the device may now use its queues.
pub fn finish<T: Transport>(transport: &T) {
    let mut state = status::ACKNOWLEDGE | status::DRIVER | status::DRIVER_OK;
    if transport.is_modern() {
        state |= status::FEATURES_OK;
    }
    transport.set_status(state);
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A transport that records what the driver did.
    pub struct Fake {
        pub modern: bool,
        pub offered: u64,
        pub accepts_features: bool,
        pub state: Cell<u32>,
        pub driver: Cell<u64>,
        pub statuses: RefCell<std::vec::Vec<u32>>,
    }

    impl Fake {
        pub fn new(modern: bool, offered: u64) -> Self {
            Self {
                modern,
                offered,
                accepts_features: true,
                state: Cell::new(1),
                driver: Cell::new(0),
                statuses: RefCell::new(std::vec::Vec::new()),
            }
        }
    }

    impl Transport for Fake {
        fn device_id(&self) -> u32 {
            device::BLOCK
        }
        fn is_modern(&self) -> bool {
            self.modern
        }
        fn status(&self) -> u32 {
            self.state.get()
        }
        fn set_status(&self, status: u32) {
            self.statuses.borrow_mut().push(status);
            let refused = status & status::FEATURES_OK != 0 && !self.accepts_features;
            self.state.set(if refused {
                status & !status::FEATURES_OK
            } else {
                status
            });
        }
        fn device_features(&self) -> u64 {
            self.offered
        }
        fn set_driver_features(&self, features: u64) {
            self.driver.set(features);
        }
        fn queue_max(&self, _index: u16) -> u16 {
            256
        }
        fn queue_activate(&self, _: u16, _: u16, _: u64, _: u64, _: u64) -> bool {
            true
        }
        fn queue_notify(&self, _index: u16) {}
        fn config_read8(&self, _offset: usize) -> u8 {
            0
        }
        fn config_write8(&self, _offset: usize, _value: u8) {}
        fn config_read32(&self, _offset: usize) -> u32 {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Fake;
    use super::*;

    #[test]
    fn a_modern_device_accepts_version_1_and_only_wanted_features() {
        let fake = Fake::new(true, F_VERSION_1 | 0b1110);
        assert_eq!(negotiate(&fake, 0b0110), Some(F_VERSION_1 | 0b0110));
        assert_eq!(fake.driver.get(), F_VERSION_1 | 0b0110);
        assert_ne!(fake.state.get() & status::FEATURES_OK, 0);
        finish(&fake);
        assert_ne!(fake.state.get() & status::DRIVER_OK, 0);
        assert_eq!(fake.statuses.borrow()[0], 0, "negotiation starts with a reset");
    }

    #[test]
    fn a_legacy_device_never_sees_a_high_feature_or_features_ok() {
        let fake = Fake::new(false, 0b11);
        assert_eq!(negotiate(&fake, F_VERSION_1 | 0b1), Some(0b1));
        assert_eq!(fake.state.get() & status::FEATURES_OK, 0);
        finish(&fake);
        assert_eq!(fake.state.get() & status::FEATURES_OK, 0);
    }

    #[test]
    fn a_device_that_refuses_is_failed() {
        let mut fake = Fake::new(true, F_VERSION_1);
        fake.accepts_features = false;
        assert_eq!(negotiate(&fake, 0), None);
        assert_eq!(fake.state.get(), status::FAILED);

        let fake = Fake::new(true, 0b1);
        assert_eq!(negotiate(&fake, 0b1), None);
    }
}
