//! The virtio-mmio transport, legacy and modern.

use core::ptr::{read_volatile, write_volatile};

use crate::Transport;

/// `"virt"`, the first register of a virtio MMIO slot.
pub const MMIO_MAGIC: u32 = 0x7472_6976;

mod reg {
    pub const MAGIC: usize = 0x000;
    pub const VERSION: usize = 0x004;
    pub const DEVICE_ID: usize = 0x008;
    pub const DEVICE_FEATURES: usize = 0x010;
    pub const DEVICE_FEATURES_SEL: usize = 0x014;
    pub const DRIVER_FEATURES: usize = 0x020;
    pub const DRIVER_FEATURES_SEL: usize = 0x024;
    pub const GUEST_PAGE_SIZE: usize = 0x028;
    pub const QUEUE_SEL: usize = 0x030;
    pub const QUEUE_NUM_MAX: usize = 0x034;
    pub const QUEUE_NUM: usize = 0x038;
    pub const QUEUE_ALIGN: usize = 0x03c;
    pub const QUEUE_PFN: usize = 0x040;
    pub const QUEUE_READY: usize = 0x044;
    pub const QUEUE_NOTIFY: usize = 0x050;
    pub const INTERRUPT_STATUS: usize = 0x060;
    pub const INTERRUPT_ACK: usize = 0x064;
    pub const STATUS: usize = 0x070;
    pub const QUEUE_DESC_LOW: usize = 0x080;
    pub const QUEUE_DESC_HIGH: usize = 0x084;
    pub const QUEUE_DRIVER_LOW: usize = 0x090;
    pub const QUEUE_DRIVER_HIGH: usize = 0x094;
    pub const QUEUE_DEVICE_LOW: usize = 0x0a0;
    pub const QUEUE_DEVICE_HIGH: usize = 0x0a4;
    pub const CONFIG: usize = 0x100;
}

/// One virtio MMIO slot.
#[derive(Clone, Copy, Debug)]
pub struct MmioTransport {
    base: usize,
    version: u32,
}

impl MmioTransport {
    /// Wraps the slot at `base`, or returns `None` when it holds no device.
    pub fn probe(base: usize) -> Option<Self> {
        let transport = Self { base, version: 0 };
        if transport.read32(reg::MAGIC) != MMIO_MAGIC {
            return None;
        }
        let version = transport.read32(reg::VERSION);
        if version != 1 && version != 2 {
            return None;
        }
        if transport.read32(reg::DEVICE_ID) == 0 {
            return None;
        }
        Some(Self { base, version })
    }

    /// Base address of the slot.
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Transport version: 1 for legacy, 2 for modern.
    pub const fn version(&self) -> u32 {
        self.version
    }

    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: `base` is a device window supplied by the board.
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: see `read32`.
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }
}

impl Transport for MmioTransport {
    fn device_id(&self) -> u32 {
        self.read32(reg::DEVICE_ID)
    }

    fn is_modern(&self) -> bool {
        self.version == 2
    }

    fn status(&self) -> u32 {
        self.read32(reg::STATUS)
    }

    fn set_status(&self, status: u32) {
        self.write32(reg::STATUS, status);
    }

    fn device_features(&self) -> u64 {
        self.write32(reg::DEVICE_FEATURES_SEL, 0);
        let low = self.read32(reg::DEVICE_FEATURES) as u64;
        if !self.is_modern() {
            return low;
        }
        self.write32(reg::DEVICE_FEATURES_SEL, 1);
        low | (self.read32(reg::DEVICE_FEATURES) as u64) << 32
    }

    fn set_driver_features(&self, features: u64) {
        self.write32(reg::DRIVER_FEATURES_SEL, 0);
        self.write32(reg::DRIVER_FEATURES, features as u32);
        if self.is_modern() {
            self.write32(reg::DRIVER_FEATURES_SEL, 1);
            self.write32(reg::DRIVER_FEATURES, (features >> 32) as u32);
        }
    }

    fn queue_max(&self, index: u16) -> u16 {
        self.write32(reg::QUEUE_SEL, index as u32);
        self.read32(reg::QUEUE_NUM_MAX).min(u16::MAX as u32) as u16
    }

    fn queue_activate(&self, index: u16, size: u16, desc: u64, avail: u64, used: u64) -> bool {
        self.write32(reg::QUEUE_SEL, index as u32);
        self.write32(reg::QUEUE_NUM, size as u32);
        if self.is_modern() {
            self.write32(reg::QUEUE_DESC_LOW, desc as u32);
            self.write32(reg::QUEUE_DESC_HIGH, (desc >> 32) as u32);
            self.write32(reg::QUEUE_DRIVER_LOW, avail as u32);
            self.write32(reg::QUEUE_DRIVER_HIGH, (avail >> 32) as u32);
            self.write32(reg::QUEUE_DEVICE_LOW, used as u32);
            self.write32(reg::QUEUE_DEVICE_HIGH, (used >> 32) as u32);
            self.write32(reg::QUEUE_READY, 1);
        } else {
            let Ok(page) = u32::try_from(desc >> 12) else {
                return false;
            };
            self.write32(reg::GUEST_PAGE_SIZE, 4096);
            self.write32(reg::QUEUE_ALIGN, 4096);
            self.write32(reg::QUEUE_PFN, page);
        }
        true
    }

    fn queue_notify(&self, index: u16) {
        self.write32(reg::QUEUE_NOTIFY, index as u32);
    }

    fn acknowledge_interrupt(&self) -> bool {
        let status = self.read32(reg::INTERRUPT_STATUS);
        if status != 0 {
            self.write32(reg::INTERRUPT_ACK, status);
        }
        status != 0
    }

    fn config_read8(&self, offset: usize) -> u8 {
        // SAFETY: see `read32`.
        unsafe { read_volatile((self.base + reg::CONFIG + offset) as *const u8) }
    }

    fn config_write8(&self, offset: usize, value: u8) {
        // SAFETY: see `read32`.
        unsafe { write_volatile((self.base + reg::CONFIG + offset) as *mut u8, value) }
    }

    fn config_read32(&self, offset: usize) -> u32 {
        self.read32(reg::CONFIG + offset)
    }
}
