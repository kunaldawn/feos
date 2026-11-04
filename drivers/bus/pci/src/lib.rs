//! Minimal PCI/PCIe enumeration over ECAM or legacy I/O ports.
//!
//! Firmware-less boot leaves PCI BARs unprogrammed, so a bare-metal kernel has
//! to assign them itself. This crate provides just enough of the config-space
//! protocol to find a device by vendor/device ID, size and assign a memory
//! BAR, and enable memory decoding plus bus mastering. It does not implement
//! interrupts, capability walking, or bridges.
//!
//! Two transports are supported:
//!
//! * [`PciEcam`] — memory-mapped config space, used by QEMU's AArch64 `virt`
//!   machine (the high ECAM window starts at `0x40_1000_0000`).
//! * [`PciPort`] — the legacy `0xcf8`/`0xcfc` I/O port pair, used on x86 and
//!   available on both i440fx and q35 without firmware tables.
//!
//! The board chooses the transport and passes the base address (or nothing,
//! for the legacy port pair).

#![cfg_attr(not(test), no_std)]

use core::ptr::{read_volatile, write_volatile};

#[cfg(target_arch = "x86_64")]
use feos_port_io::{inl, outl};

/// Number of buses scanned by `find`.
const SCAN_BUSES: u8 = 8;

/// PCI configuration space registers used by this crate.
mod reg {
    pub const VENDOR_ID: usize = 0x00;
    pub const COMMAND: usize = 0x04;
    pub const HEADER_TYPE: usize = 0x0e;
    pub const BAR0: usize = 0x10;
}

/// BAR type field: bits 2:1 of a memory BAR.
const BAR_TYPE_MASK: u32 = 0b110;
/// BAR type field value for a 64-bit BAR.
const BAR_TYPE_64: u32 = 0b100;

/// PCI command-register bits.
mod command {
    pub const MEMORY_SPACE: u16 = 1 << 1;
    pub const BUS_MASTER: u16 = 1 << 2;
}

/// Access to one PCI configuration space.
pub trait ConfigSpace: Copy {
    /// Reads a 32-bit configuration-space register.
    fn read32(&self, bus: u8, device: u8, function: u8, offset: usize) -> u32;

    /// Writes a 32-bit configuration-space register.
    fn write32(&self, bus: u8, device: u8, function: u8, offset: usize, value: u32);

    /// Reads a 16-bit configuration-space register.
    fn read16(&self, bus: u8, device: u8, function: u8, offset: usize) -> u16 {
        let aligned = offset & !3;
        let value = self.read32(bus, device, function, aligned);
        (value >> ((offset & 2) * 8)) as u16
    }

    /// Writes a 16-bit configuration-space register.
    fn write16(&self, bus: u8, device: u8, function: u8, offset: usize, value: u16) {
        let aligned = offset & !3;
        let shift = (offset & 2) * 8;
        let mask = !(0xffffu32 << shift);
        let current = self.read32(bus, device, function, aligned);
        self.write32(
            bus,
            device,
            function,
            aligned,
            (current & mask) | ((value as u32) << shift),
        );
    }

    /// Reads an 8-bit configuration-space register.
    fn read8(&self, bus: u8, device: u8, function: u8, offset: usize) -> u8 {
        let aligned = offset & !3;
        let value = self.read32(bus, device, function, aligned);
        (value >> ((offset & 3) * 8)) as u8
    }

    /// Writes an 8-bit configuration-space register.
    fn write8(&self, bus: u8, device: u8, function: u8, offset: usize, value: u8) {
        let aligned = offset & !3;
        let shift = (offset & 3) * 8;
        let mask = !(0xffu32 << shift);
        let current = self.read32(bus, device, function, aligned);
        self.write32(
            bus,
            device,
            function,
            aligned,
            (current & mask) | ((value as u32) << shift),
        );
    }
}

/// Finds the first device with `vendor`/`device` on buses 0..8.
fn find_device<C: ConfigSpace>(
    config: C,
    vendor: u16,
    device: u16,
) -> Option<PciDevice<C>> {
    for bus in 0..SCAN_BUSES {
        for slot in 0..32 {
            for function in 0..8 {
                let id = config.read32(bus, slot, function, reg::VENDOR_ID);
                if id == 0xffff_ffff {
                    // No function present (and no further functions on this
                    // slot for a single-function device).
                    if function == 0 {
                        break;
                    }
                    continue;
                }

                if id as u16 == vendor && (id >> 16) as u16 == device {
                    return Some(PciDevice {
                        config,
                        bus,
                        slot,
                        function,
                    });
                }

                // Only function 0 is guaranteed to exist; functions 1..7 may
                // be present on a multi-function device.
                let header = config.read8(bus, slot, function, reg::HEADER_TYPE);
                if function == 0 && header & 0x80 == 0 {
                    break;
                }
            }
        }
    }

    None
}

/// A PCIe ECAM window.
#[derive(Clone, Copy, Debug)]
pub struct PciEcam {
    base: usize,
}

impl PciEcam {
    /// Creates an ECAM accessor for the window at `base`.
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    /// Base address of the ECAM window.
    pub const fn base(&self) -> usize {
        self.base
    }

    fn address(&self, bus: u8, device: u8, function: u8, offset: usize) -> usize {
        self.base
            + ((bus as usize) << 20)
            + ((device as usize) << 15)
            + ((function as usize) << 12)
            + offset
    }

    /// Finds the first device with `vendor`/`device` on buses 0..8.
    pub fn find(&self, vendor: u16, device: u16) -> Option<PciDevice<Self>> {
        find_device(*self, vendor, device)
    }
}

impl ConfigSpace for PciEcam {
    fn read32(&self, bus: u8, device: u8, function: u8, offset: usize) -> u32 {
        // SAFETY: `base` is an MMIO window supplied by the board and the
        // callers pass a device/function address inside it.
        unsafe { read_volatile(self.address(bus, device, function, offset) as *const u32) }
    }

    fn write32(&self, bus: u8, device: u8, function: u8, offset: usize, value: u32) {
        // SAFETY: see `read32`.
        unsafe {
            write_volatile(
                self.address(bus, device, function, offset) as *mut u32,
                value,
            )
        }
    }
}

/// Legacy PCI configuration access through I/O ports `0xcf8`/`0xcfc`.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Debug)]
pub struct PciPort;

#[cfg(target_arch = "x86_64")]
impl PciPort {
    /// Creates a legacy-port accessor.
    pub const fn new() -> Self {
        Self
    }

    fn address(bus: u8, device: u8, function: u8, offset: usize) -> u32 {
        0x8000_0000
            | ((bus as u32) << 16)
            | ((device as u32) << 11)
            | ((function as u32) << 8)
            | (offset as u32 & 0xfc)
    }

    /// Finds the first device with `vendor`/`device` on buses 0..8.
    pub fn find(&self, vendor: u16, device: u16) -> Option<PciDevice<Self>> {
        find_device(*self, vendor, device)
    }
}

#[cfg(target_arch = "x86_64")]
impl ConfigSpace for PciPort {
    fn read32(&self, bus: u8, device: u8, function: u8, offset: usize) -> u32 {
        let address = Self::address(bus, device, function, offset);
        // SAFETY: `0xcf8`/`0xcfc` are the standard PCI configuration ports.
        unsafe {
            outl(0xcf8, address);
            inl(0xcfc)
        }
    }

    fn write32(&self, bus: u8, device: u8, function: u8, offset: usize, value: u32) {
        let address = Self::address(bus, device, function, offset);
        // SAFETY: `0xcf8`/`0xcfc` are the standard PCI configuration ports.
        unsafe {
            outl(0xcf8, address);
            outl(0xcfc, value);
        }
    }
}

/// One PCI function located by a scan.
#[derive(Clone, Copy, Debug)]
pub struct PciDevice<C: ConfigSpace> {
    config: C,
    bus: u8,
    slot: u8,
    function: u8,
}

impl<C: ConfigSpace> PciDevice<C> {
    /// Bus number of this function.
    pub const fn bus(&self) -> u8 {
        self.bus
    }

    /// Slot (device) number of this function.
    pub const fn slot(&self) -> u8 {
        self.slot
    }

    /// Function number.
    pub const fn function(&self) -> u8 {
        self.function
    }

    /// Vendor ID.
    pub fn vendor_id(&self) -> u16 {
        self.config
            .read32(self.bus, self.slot, self.function, reg::VENDOR_ID) as u16
    }

    /// Device ID.
    pub fn device_id(&self) -> u16 {
        (self
            .config
            .read32(self.bus, self.slot, self.function, reg::VENDOR_ID)
            >> 16) as u16
    }

    /// Class code (bits 31:8 of the class/revision register).
    pub fn class_code(&self) -> u32 {
        self.config
            .read32(self.bus, self.slot, self.function, 0x08)
            >> 8
    }

    /// Reads a 32-bit BAR register (index 0..5).
    pub fn bar(&self, index: usize) -> u32 {
        self.config
            .read32(self.bus, self.slot, self.function, reg::BAR0 + index * 4)
    }

    fn read_bar(&self, offset: usize) -> u32 {
        self.config
            .read32(self.bus, self.slot, self.function, offset)
    }

    fn write_bar(&self, offset: usize, value: u32) {
        self.config
            .write32(self.bus, self.slot, self.function, offset, value);
    }

    /// Returns `true` when a memory BAR's type field says it is 64 bits wide.
    fn is_64_bit(low: u32) -> bool {
        low & BAR_TYPE_MASK == BAR_TYPE_64
    }

    /// Runs `body` with memory decoding off.
    ///
    /// While a BAR is being sized or moved it can hold any address, and the
    /// device would answer accesses meant for something else.
    fn with_decode_disabled<T>(&self, body: impl FnOnce() -> T) -> T {
        let current = self
            .config
            .read16(self.bus, self.slot, self.function, reg::COMMAND);
        self.config.write16(
            self.bus,
            self.slot,
            self.function,
            reg::COMMAND,
            current & !command::MEMORY_SPACE,
        );
        let result = body();
        self.config
            .write16(self.bus, self.slot, self.function, reg::COMMAND, current);
        result
    }

    /// Returns the address programmed into a memory BAR.
    ///
    /// Returns `None` for an I/O BAR. An address of zero means firmware left the
    /// BAR unassigned.
    pub fn memory_bar_address(&self, index: usize) -> Option<u64> {
        let offset = reg::BAR0 + index * 4;
        let low = self.read_bar(offset);
        if low & 1 != 0 {
            return None;
        }
        let high = if Self::is_64_bit(low) {
            self.read_bar(offset + 4)
        } else {
            0
        };
        Some(((high as u64) << 32) | (low & !0xf) as u64)
    }

    /// Sizes a BAR and returns its length in bytes.
    ///
    /// Returns `None` for I/O BARs or if the size cannot be probed.
    pub fn bar_size(&self, index: usize) -> Option<u64> {
        let offset = reg::BAR0 + index * 4;
        let original = self.read_bar(offset);

        if original & 1 != 0 {
            return None; // I/O space BAR
        }

        let wide = Self::is_64_bit(original);
        let mask = self.with_decode_disabled(|| {
            self.write_bar(offset, 0xffff_ffff);
            let low = self.read_bar(offset);
            self.write_bar(offset, original);
            let high = if wide {
                let high_original = self.read_bar(offset + 4);
                self.write_bar(offset + 4, 0xffff_ffff);
                let high = self.read_bar(offset + 4);
                self.write_bar(offset + 4, high_original);
                high
            } else {
                // A 32-bit BAR decodes nothing above bit 31.
                0xffff_ffff
            };
            ((high as u64) << 32) | (low & !0xf) as u64
        });

        if mask as u32 == 0 && mask >> 32 == if wide { 0 } else { 0xffff_ffff } {
            return None;
        }

        Some((!mask).wrapping_add(1))
    }

    /// Assigns `address` to a memory BAR.
    ///
    /// Handles both 32-bit and 64-bit memory BARs. Returns `false` if the BAR
    /// is not memory-mapped, has an unknown size, `address` is misaligned, or
    /// `address` doesn't fit a 32-bit BAR.
    pub fn assign_memory_bar(&self, index: usize, address: u64) -> bool {
        let offset = reg::BAR0 + index * 4;
        let original = self.read_bar(offset);

        if original & 1 != 0 {
            return false;
        }

        let Some(size) = self.bar_size(index) else {
            return false;
        };

        let wide = Self::is_64_bit(original);
        if !address.is_multiple_of(size) || (!wide && address > u32::MAX as u64) {
            return false;
        }

        self.with_decode_disabled(|| {
            // Preserve the read-only type bits (bit 0 is 0 for memory, bits 2:1
            // encode 32/64-bit addressing and bit 3 prefetchability).
            self.write_bar(offset, (address as u32) | (original & 0xf));
            if wide {
                // 64-bit BAR: the upper half lives in the next register.
                self.write_bar(offset + 4, (address >> 32) as u32);
            }
        });

        true
    }

    /// Enables memory-space decoding and bus mastering.
    pub fn enable(&self) {
        let current = self.config.read16(
            self.bus,
            self.slot,
            self.function,
            reg::COMMAND,
        );
        self.config.write16(
            self.bus,
            self.slot,
            self.function,
            reg::COMMAND,
            current | command::MEMORY_SPACE | command::BUS_MASTER,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A config space backed by a map, with a sizable BAR per function.
    struct Bus {
        registers: RefCell<HashMap<(u8, u8, u8, usize), u32>>,
        /// `(bus, slot, function, bar offset) -> writable address mask`.
        masks: RefCell<HashMap<(u8, u8, u8, usize), u32>>,
        /// Each write to a BAR while memory decoding is on.
        writes_while_decoding: RefCell<usize>,
    }

    impl Bus {
        fn new() -> &'static Self {
            Box::leak(Box::new(Self {
                registers: RefCell::new(HashMap::new()),
                masks: RefCell::new(HashMap::new()),
                writes_while_decoding: RefCell::new(0),
            }))
        }

        fn add(&self, slot: u8, function: u8, id: u32, multi_function: bool) {
            let mut registers = self.registers.borrow_mut();
            registers.insert((0, slot, function, reg::VENDOR_ID), id);
            let header = if multi_function { 0x80u32 << 16 } else { 0 };
            registers.insert((0, slot, function, 0x0c), header);
            // Cache line size in the byte the wrong offset used to read.
            registers.entry((0, slot, function, 0x0c)).and_modify(|value| *value |= 0x10);
        }

        fn bar(&self, slot: u8, function: u8, offset: usize, value: u32, mask: u32) {
            self.registers
                .borrow_mut()
                .insert((0, slot, function, offset), value);
            self.masks
                .borrow_mut()
                .insert((0, slot, function, offset), mask);
        }
    }

    impl ConfigSpace for &'static Bus {
        fn read32(&self, bus: u8, device: u8, function: u8, offset: usize) -> u32 {
            *self
                .registers
                .borrow()
                .get(&(bus, device, function, offset))
                .unwrap_or(&if offset == reg::VENDOR_ID {
                    0xffff_ffff
                } else {
                    0
                })
        }

        fn write32(&self, bus: u8, device: u8, function: u8, offset: usize, value: u32) {
            let key = (bus, device, function, offset);
            let value = match self.masks.borrow().get(&key) {
                Some(mask) => {
                    let command = self.read32(bus, device, function, reg::COMMAND);
                    if command as u16 & command::MEMORY_SPACE != 0 {
                        *self.writes_while_decoding.borrow_mut() += 1;
                    }
                    let current = self.read32(bus, device, function, offset);
                    (value & mask) | (current & !mask)
                }
                None => value,
            };
            self.registers.borrow_mut().insert(key, value);
        }
    }

    const ID: u32 = 0x2668_8086;

    fn found(bus: &'static Bus) -> Option<PciDevice<&'static Bus>> {
        find_device(bus, 0x8086, 0x2668)
    }

    #[test]
    fn scan_finds_a_device_at_function_zero() {
        let bus = Bus::new();
        bus.add(3, 0, ID, false);
        let device = found(bus).expect("device");
        assert_eq!((device.slot(), device.function()), (3, 0));
    }

    #[test]
    fn scan_reaches_higher_functions_of_a_multi_function_device() {
        // DRV-6: the header type was read from 0x0c, the cache line size, so
        // the multi-function bit was never seen. q35's AHCI is at 00:1f.2.
        let bus = Bus::new();
        bus.add(0x1f, 0, 0x2918_8086, true);
        bus.add(0x1f, 2, ID, false);
        let device = found(bus).expect("device");
        assert_eq!((device.slot(), device.function()), (0x1f, 2));
    }

    #[test]
    fn scan_skips_functions_of_a_single_function_device() {
        let bus = Bus::new();
        bus.add(1, 0, 0x1234_5678, false);
        bus.add(1, 2, ID, false);
        assert!(found(bus).is_none());
    }

    #[test]
    fn read8_and_read16_select_the_right_lane() {
        let bus = Bus::new();
        bus.registers.borrow_mut().insert((0, 0, 0, 0x0c), 0x0080_0010);
        assert_eq!(bus.read8(0, 0, 0, 0x0c), 0x10);
        assert_eq!(bus.read8(0, 0, 0, 0x0e), 0x80);
        assert_eq!(bus.read16(0, 0, 0, 0x0e), 0x0080);
    }

    #[test]
    fn a_32_bit_bar_sizes_and_assigns() {
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, 0, 0xffff_c000);
        let device = found(bus).unwrap();
        assert_eq!(device.bar_size(0), Some(0x4000));
        assert!(device.assign_memory_bar(0, 0xc000_0000));
        assert_eq!(device.memory_bar_address(0), Some(0xc000_0000));

        // A 32-bit BAR can't hold a 64-bit address, and alignment is enforced.
        assert!(!device.assign_memory_bar(0, 0x1_0000_0000));
        assert!(!device.assign_memory_bar(0, 0xc000_1000));
    }

    #[test]
    fn a_64_bit_bar_takes_an_address_above_4_gib() {
        // DRV-17: the docs claimed 64-bit BARs while the code refused them.
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, BAR_TYPE_64, 0xffff_c000);
        bus.bar(1, 0, reg::BAR0 + 4, 0, 0xffff_ffff);
        let device = found(bus).unwrap();
        assert_eq!(device.bar_size(0), Some(0x4000));
        assert!(device.assign_memory_bar(0, 0x2_c000_0000));
        assert_eq!(device.memory_bar_address(0), Some(0x2_c000_0000));
    }

    #[test]
    fn sizing_restores_the_bar_and_decode_is_off_meanwhile() {
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, 0xc000_0000, 0xffff_c000);
        let device = found(bus).unwrap();
        device.enable();

        assert_eq!(device.bar_size(0), Some(0x4000));
        assert_eq!(device.memory_bar_address(0), Some(0xc000_0000));
        assert!(device.assign_memory_bar(0, 0xd000_0000));
        assert_eq!(*bus.writes_while_decoding.borrow(), 0);

        // Decoding is back on afterwards.
        let command = bus.read16(0, 1, 0, reg::COMMAND);
        assert_ne!(command & command::MEMORY_SPACE, 0);
    }

    #[test]
    fn an_io_bar_is_refused() {
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, 0xc001, 0xffff_fff0);
        let device = found(bus).unwrap();
        assert_eq!(device.bar_size(0), None);
        assert_eq!(device.memory_bar_address(0), None);
        assert!(!device.assign_memory_bar(0, 0xc000_0000));
    }

    #[test]
    fn an_unimplemented_bar_has_no_size() {
        let bus = Bus::new();
        bus.add(1, 0, ID, false);
        bus.bar(1, 0, reg::BAR0, 0, 0);
        let device = found(bus).unwrap();
        assert_eq!(device.bar_size(0), None);
    }
}
