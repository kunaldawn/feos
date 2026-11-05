//! The modern virtio-pci transport.
//!
//! A modern function describes where its register blocks live with
//! vendor-specific capabilities: which BAR, what offset. Transitional functions
//! offer these too, next to their legacy I/O BAR, so nothing here needs port I/O.

use core::ptr::{read_volatile, write_volatile};

use feos_driver_pci::{bridge_window, BarAllocator, ConfigSpace, PciDevice};

use crate::Transport;

/// Red Hat's vendor ID, shared by all virtio functions.
const VENDOR: u16 = 0x1af4;
/// Modern device IDs are this plus the virtio device ID.
const MODERN_BASE: u16 = 0x1040;
/// Transitional device IDs start here, in the order of this table.
const TRANSITIONAL_BASE: u16 = 0x1000;
const TRANSITIONAL_IDS: [u32; 10] = [1, 2, 5, 3, 8, 4, 0, 0, 9, 0];

/// Capability ID of a vendor-specific capability.
const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_ISR: u8 = 3;
const CFG_DEVICE: u8 = 4;

/// Longest capability length a block can claim: a BAR is at most this long here.
const MAX_BLOCK: u32 = 1 << 26;

mod common {
    pub const DEVICE_FEATURE_SELECT: usize = 0x00;
    pub const DEVICE_FEATURE: usize = 0x04;
    pub const DRIVER_FEATURE_SELECT: usize = 0x08;
    pub const DRIVER_FEATURE: usize = 0x0c;
    pub const MSIX_CONFIG: usize = 0x10;
    pub const STATUS: usize = 0x14;
    pub const QUEUE_SELECT: usize = 0x16;
    pub const QUEUE_SIZE: usize = 0x18;
    pub const QUEUE_MSIX_VECTOR: usize = 0x1a;
    pub const QUEUE_ENABLE: usize = 0x1c;
    pub const QUEUE_NOTIFY_OFF: usize = 0x1e;
    pub const QUEUE_DESC: usize = 0x20;
    pub const QUEUE_DRIVER: usize = 0x28;
    pub const QUEUE_DEVICE: usize = 0x30;
    /// Bytes of the common block this driver touches.
    pub const LEN: u32 = 0x38;
}

/// One modern virtio-pci function.
#[derive(Clone, Copy, Debug)]
pub struct PciTransport {
    device_id: u32,
    common: usize,
    notify: usize,
    notify_multiplier: u32,
    device: usize,
    /// The block whose first byte reads and clears the interrupt status.
    isr: usize,
    /// Where the function's MSI-X table is, if it has one.
    msix_table: Option<usize>,
}

/// Written to a vector register for "no MSI-X entry".
const NO_VECTOR: u16 = 0xffff;

/// The virtio device ID of a PCI function, or `None` if it isn't virtio.
fn virtio_id<C: ConfigSpace>(function: &PciDevice<C>) -> Option<u32> {
    if function.vendor_id() != VENDOR {
        return None;
    }
    let id = function.device_id();
    if id >= MODERN_BASE {
        return Some((id - MODERN_BASE) as u32);
    }
    let index = id.checked_sub(TRANSITIONAL_BASE)? as usize;
    match TRANSITIONAL_IDS.get(index) {
        Some(&id) if id != 0 => Some(id),
        _ => None,
    }
}

impl PciTransport {
    /// True when `function` is a virtio function for device type `wanted`.
    pub fn matches<C: ConfigSpace>(function: &PciDevice<C>, wanted: u32) -> bool {
        virtio_id(function) == Some(wanted)
    }

    /// Reads the virtio capabilities of `function`, gives its BARs addresses
    /// and turns on decoding and bus mastering.
    ///
    /// A device behind a bridge is allocated from that bridge's window, which is
    /// the only range that reaches it. Returns `None` if `function` isn't a
    /// modern-capable virtio function or its capabilities don't make sense.
    pub fn new<C: ConfigSpace>(
        config: C,
        function: PciDevice<C>,
        allocator: &mut BarAllocator,
    ) -> Option<Self> {
        let device_id = virtio_id(&function)?;
        let mut behind_bridge = bridge_window(config, function.bus())
            .map(|(base, size)| BarAllocator::new(base, base + size));
        let allocator = behind_bridge.as_mut().unwrap_or(allocator);

        let mut common = None;
        let mut notify = None;
        let mut multiplier = 0u32;
        let mut device = None;
        let mut isr = 0;

        for capability in function.capabilities() {
            if capability.id != CAP_VENDOR {
                continue;
            }
            let at = capability.offset;
            let kind = function.config_read8(at + 3);
            if !matches!(kind, CFG_COMMON | CFG_NOTIFY | CFG_DEVICE | CFG_ISR) {
                continue;
            }
            let bar = function.config_read8(at + 4) as usize;
            let offset = function.config_read32(at + 8);
            let length = function.config_read32(at + 12);
            if bar > 5 || length == 0 || length > MAX_BLOCK {
                continue;
            }
            let size = function.bar_size(bar)?;
            if (offset as u64).checked_add(length as u64)? > size {
                continue;
            }
            let base = function.ensure_memory_bar(bar, allocator)?;
            let address = usize::try_from(base.checked_add(offset as u64)?).ok()?;
            match kind {
                CFG_COMMON if length >= common::LEN => common = Some(address),
                CFG_NOTIFY => {
                    notify = Some(address);
                    multiplier = function.config_read32(at + 16);
                }
                CFG_DEVICE => device = Some(address),
                CFG_ISR => isr = address,
                _ => {}
            }
        }

        // Its table is in a BAR of its own on QEMU, which needs an address too.
        let msix_table = function.msix_table(allocator);

        function.enable();
        Some(Self {
            msix_table,
            device_id,
            common: common?,
            notify: notify?,
            notify_multiplier: multiplier,
            // A device with no configuration has no such capability.
            device: device.unwrap_or(0),
            isr,
        })
    }

    /// Where the function's MSI-X table is, if it has one.
    pub const fn msix_table(&self) -> Option<usize> {
        self.msix_table
    }

    /// Points a queue's interrupts at an entry of the function's MSI-X table.
    /// Returns `false` if the device refused it.
    pub fn set_queue_vector(&self, queue: u16, entry: u16) -> bool {
        self.write16(common::QUEUE_SELECT, queue);
        self.write16(common::QUEUE_MSIX_VECTOR, entry);
        self.read16(common::QUEUE_MSIX_VECTOR) == entry
    }

    /// Stops the device's configuration-change interrupts: nothing here needs them.
    pub fn clear_config_vector(&self) {
        self.write16(common::MSIX_CONFIG, NO_VECTOR);
    }

    fn read8(&self, offset: usize) -> u8 {
        // SAFETY: `new` checked that the common block fits its BAR.
        unsafe { read_volatile((self.common + offset) as *const u8) }
    }

    fn write8(&self, offset: usize, value: u8) {
        // SAFETY: see `read8`.
        unsafe { write_volatile((self.common + offset) as *mut u8, value) }
    }

    fn read16(&self, offset: usize) -> u16 {
        // SAFETY: see `read8`.
        unsafe { read_volatile((self.common + offset) as *const u16) }
    }

    fn write16(&self, offset: usize, value: u16) {
        // SAFETY: see `read8`.
        unsafe { write_volatile((self.common + offset) as *mut u16, value) }
    }

    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: see `read8`.
        unsafe { read_volatile((self.common + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: see `read8`.
        unsafe { write_volatile((self.common + offset) as *mut u32, value) }
    }

    fn write64(&self, offset: usize, value: u64) {
        self.write32(offset, value as u32);
        self.write32(offset + 4, (value >> 32) as u32);
    }
}

impl Transport for PciTransport {
    fn device_id(&self) -> u32 {
        self.device_id
    }

    fn is_modern(&self) -> bool {
        true
    }

    fn status(&self) -> u32 {
        self.read8(common::STATUS) as u32
    }

    fn set_status(&self, status: u32) {
        self.write8(common::STATUS, status as u8);
    }

    fn device_features(&self) -> u64 {
        self.write32(common::DEVICE_FEATURE_SELECT, 0);
        let low = self.read32(common::DEVICE_FEATURE) as u64;
        self.write32(common::DEVICE_FEATURE_SELECT, 1);
        low | (self.read32(common::DEVICE_FEATURE) as u64) << 32
    }

    fn set_driver_features(&self, features: u64) {
        self.write32(common::DRIVER_FEATURE_SELECT, 0);
        self.write32(common::DRIVER_FEATURE, features as u32);
        self.write32(common::DRIVER_FEATURE_SELECT, 1);
        self.write32(common::DRIVER_FEATURE, (features >> 32) as u32);
    }

    fn queue_max(&self, index: u16) -> u16 {
        self.write16(common::QUEUE_SELECT, index);
        self.read16(common::QUEUE_SIZE)
    }

    fn queue_activate(&self, index: u16, size: u16, desc: u64, avail: u64, used: u64) -> bool {
        self.write16(common::QUEUE_SELECT, index);
        self.write16(common::QUEUE_SIZE, size);
        self.write64(common::QUEUE_DESC, desc);
        self.write64(common::QUEUE_DRIVER, avail);
        self.write64(common::QUEUE_DEVICE, used);
        self.write16(common::QUEUE_ENABLE, 1);
        true
    }

    fn queue_notify(&self, index: u16) {
        self.write16(common::QUEUE_SELECT, index);
        let slot = self.read16(common::QUEUE_NOTIFY_OFF) as usize;
        let address = self.notify + slot * self.notify_multiplier as usize;
        // SAFETY: the capability placed the notify block, and the device chose
        // this queue's slot inside it.
        unsafe { write_volatile(address as *mut u16, index) }
    }

    fn acknowledge_interrupt(&self) -> bool {
        if self.isr == 0 {
            return false;
        }
        // SAFETY: the capability placed the block, and reading it is what clears it.
        unsafe { read_volatile(self.isr as *const u8) != 0 }
    }

    fn config_read8(&self, offset: usize) -> u8 {
        if self.device == 0 {
            return 0;
        }
        // SAFETY: the capability placed the device block.
        unsafe { read_volatile((self.device + offset) as *const u8) }
    }

    fn config_write8(&self, offset: usize, value: u8) {
        if self.device != 0 {
            // SAFETY: see `config_read8`.
            unsafe { write_volatile((self.device + offset) as *mut u8, value) }
        }
    }

    fn config_read32(&self, offset: usize) -> u32 {
        if self.device == 0 {
            return 0;
        }
        // SAFETY: see `config_read8`.
        unsafe { read_volatile((self.device + offset) as *const u32) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transitional_table_follows_the_specification() {
        // virtio-blk's transitional ID is 0x1001 and virtio-net's 0x1000.
        assert_eq!(TRANSITIONAL_IDS[0], 1);
        assert_eq!(TRANSITIONAL_IDS[1], crate::device::BLOCK);
    }
}
