//! Minimal PCI/PCIe enumeration over an ECAM (memory-mapped config space).
//!
//! Firmware-less boot leaves PCI BARs unprogrammed, so a bare-metal kernel has
//! to assign them itself. This crate provides just enough of the config-space
//! protocol to find a device by vendor/device ID, size and assign a memory
//! BAR, and enable memory decoding plus bus mastering. It does not implement
//! interrupts, capability walking, or bridges.
//!
//! The board supplies the ECAM base address, which is machine-specific. On
//! QEMU's `aarch64 virt` machine the high ECAM window starts at
//! `0x40_1000_0000`.

#![no_std]

use core::ptr::{read_volatile, write_volatile};

/// Number of buses scanned by [`PciEcam::find`].
const SCAN_BUSES: u8 = 8;

/// PCI configuration space registers used by this crate.
mod reg {
    pub const VENDOR_ID: usize = 0x00;
    pub const COMMAND: usize = 0x04;
    pub const HEADER_TYPE: usize = 0x0c;
    pub const BAR0: usize = 0x10;
}

/// PCI command-register bits.
mod command {
    pub const MEMORY_SPACE: u16 = 1 << 1;
    pub const BUS_MASTER: u16 = 1 << 2;
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

    /// Reads a 32-bit configuration-space register.
    pub fn read32(&self, bus: u8, device: u8, function: u8, offset: usize) -> u32 {
        // SAFETY: `base` is an MMIO window supplied by the board and the callers
        // pass a device/function address inside it.
        unsafe { read_volatile(self.address(bus, device, function, offset) as *const u32) }
    }

    /// Writes a 32-bit configuration-space register.
    pub fn write32(&self, bus: u8, device: u8, function: u8, offset: usize, value: u32) {
        // SAFETY: see `read32`.
        unsafe {
            write_volatile(
                self.address(bus, device, function, offset) as *mut u32,
                value,
            )
        }
    }

    /// Reads a 16-bit configuration-space register.
    pub fn read16(&self, bus: u8, device: u8, function: u8, offset: usize) -> u16 {
        // SAFETY: see `read32`.
        unsafe { read_volatile(self.address(bus, device, function, offset) as *const u16) }
    }

    /// Writes a 16-bit configuration-space register.
    pub fn write16(&self, bus: u8, device: u8, function: u8, offset: usize, value: u16) {
        // SAFETY: see `read32`.
        unsafe {
            write_volatile(
                self.address(bus, device, function, offset) as *mut u16,
                value,
            )
        }
    }

    /// Reads an 8-bit configuration-space register.
    pub fn read8(&self, bus: u8, device: u8, function: u8, offset: usize) -> u8 {
        // SAFETY: see `read32`.
        unsafe { read_volatile(self.address(bus, device, function, offset) as *const u8) }
    }

    /// Finds the first device with `vendor`/`device` on buses 0..8.
    pub fn find(&self, vendor: u16, device: u16) -> Option<PciDevice> {
        for bus in 0..SCAN_BUSES {
            for slot in 0..32 {
                for function in 0..8 {
                    let id = self.read32(bus, slot, function, reg::VENDOR_ID);
                    if id == 0xffff_ffff {
                        // No function present (and no further functions on
                        // this slot for a single-function device).
                        if function == 0 {
                            break;
                        }
                        continue;
                    }

                    if id as u16 == vendor && (id >> 16) as u16 == device {
                        return Some(PciDevice {
                            ecam: *self,
                            bus,
                            slot,
                            function,
                        });
                    }

                    // Only function 0 is guaranteed to exist; functions 1..7
                    // may be present on a multi-function device.
                    let header = self.read8(bus, slot, function, reg::HEADER_TYPE);
                    if function == 0 && header & 0x80 == 0 {
                        break;
                    }
                }
            }
        }

        None
    }
}

/// One PCI function located by [`PciEcam::find`].
#[derive(Clone, Copy, Debug)]
pub struct PciDevice {
    ecam: PciEcam,
    bus: u8,
    slot: u8,
    function: u8,
}

impl PciDevice {
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
        self.ecam
            .read32(self.bus, self.slot, self.function, reg::VENDOR_ID) as u16
    }

    /// Device ID.
    pub fn device_id(&self) -> u16 {
        (self
            .ecam
            .read32(self.bus, self.slot, self.function, reg::VENDOR_ID)
            >> 16) as u16
    }

    /// Class code (bits 31:8 of the class/revision register).
    pub fn class_code(&self) -> u32 {
        self.ecam.read32(self.bus, self.slot, self.function, 0x08) >> 8
    }

    /// Reads a 32-bit BAR register (index 0..5).
    pub fn bar(&self, index: usize) -> u32 {
        self.ecam
            .read32(self.bus, self.slot, self.function, reg::BAR0 + index * 4)
    }

    /// Sizes a BAR and returns its length in bytes.
    ///
    /// Returns `None` for I/O BARs or if the size cannot be probed.
    pub fn bar_size(&self, index: usize) -> Option<u64> {
        let offset = reg::BAR0 + index * 4;
        let original = self.ecam.read32(self.bus, self.slot, self.function, offset);

        if original & 1 != 0 {
            return None; // I/O space BAR
        }

        self.ecam
            .write32(self.bus, self.slot, self.function, offset, 0xffff_ffff);
        let mask = self.ecam.read32(self.bus, self.slot, self.function, offset);
        self.ecam
            .write32(self.bus, self.slot, self.function, offset, original);

        let mask = mask & !0xf;
        if mask == 0 {
            return None;
        }

        Some((!mask as u64) + 1)
    }

    /// Assigns `address` to a memory BAR and enables decoding.
    ///
    /// Handles both 32-bit and 64-bit memory BARs. Returns `false` if the BAR
    /// is not memory-mapped, has an unknown size, or `address` is misaligned.
    pub fn assign_memory_bar(&self, index: usize, address: u64) -> bool {
        let offset = reg::BAR0 + index * 4;
        let original = self.ecam.read32(self.bus, self.slot, self.function, offset);

        if original & 1 != 0 {
            return false;
        }

        let Some(size) = self.bar_size(index) else {
            return false;
        };

        if !address.is_multiple_of(size) || address > u32::MAX as u64 {
            return false;
        }

        // Preserve the read-only type bits (bit 0 is 0 for memory, bits 2:1
        // encode 32/64-bit addressing and prefetchability).
        self.ecam.write32(
            self.bus,
            self.slot,
            self.function,
            offset,
            (address as u32) | (original & 0xf),
        );

        if original & 0x4 != 0 {
            // 64-bit BAR: the upper half lives in the next register.
            self.ecam
                .write32(self.bus, self.slot, self.function, offset + 4, 0);
        }

        true
    }

    /// Enables memory-space decoding and bus mastering.
    pub fn enable(&self) {
        let current = self
            .ecam
            .read16(self.bus, self.slot, self.function, reg::COMMAND);
        self.ecam.write16(
            self.bus,
            self.slot,
            self.function,
            reg::COMMAND,
            current | command::MEMORY_SPACE | command::BUS_MASTER,
        );
    }
}
