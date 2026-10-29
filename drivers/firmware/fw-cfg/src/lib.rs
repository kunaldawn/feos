//! QEMU fw_cfg driver (MMIO + DMA), limited to what the framebuffer needs.
//!
//! On the `virt` machine fw_cfg is mapped at `0x0902_0000`:
//!
//! | offset | register                                  |
//! |--------|-------------------------------------------|
//! | 0x00   | data (8-bit accesses used here)           |
//! | 0x08   | selector (16-bit, big-endian)             |
//! | 0x10   | DMA address (64-bit, big-endian)          |
//!
//! The DMA transfer descriptor is also big-endian. See the QEMU fw_cfg
//! specification for details.
//!
//! The driver is stateless apart from its MMIO base address; other drivers
//! (such as `feos-driver-ramfb`) take an [`FwCfg`] value and use it.

#![no_std]

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};

const DATA: usize = 0x00;
const SELECTOR: usize = 0x08;
const DMA_ADDRESS_HIGH: usize = 0x10;
const DMA_ADDRESS_LOW: usize = 0x14;

const SIGNATURE: u16 = 0x0000;
const FILE_DIR: u16 = 0x0019;

const CONTROL_ERROR: u32 = 1 << 0;
const CONTROL_SELECT: u32 = 1 << 3;
const CONTROL_WRITE: u32 = 1 << 4;

/// An entry in the fw_cfg file directory.
#[derive(Clone, Copy, Debug)]
pub struct FileEntry {
    /// Size of the item in bytes.
    pub size: u32,
    /// fw_cfg selector used to read or write the item.
    pub selector: u16,
}

/// A QEMU fw_cfg device at a given MMIO base address.
#[derive(Clone, Copy, Debug)]
pub struct FwCfg {
    base: usize,
}

impl FwCfg {
    /// Creates a driver for the device mapped at `base`.
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    /// Base address of the device.
    pub const fn base(&self) -> usize {
        self.base
    }

    fn select(&self, key: u16) {
        // The MMIO selector register is big-endian.
        // SAFETY: `base` is an MMIO address supplied by the board.
        unsafe { write_volatile((self.base + SELECTOR) as *mut u16, key.to_be()) };
    }

    fn read_u8(&self) -> u8 {
        // SAFETY: `base` is an MMIO address supplied by the board.
        unsafe { read_volatile((self.base + DATA) as *const u8) }
    }

    fn read_be_u16(&self) -> u16 {
        u16::from_be_bytes([self.read_u8(), self.read_u8()])
    }

    fn read_be_u32(&self) -> u32 {
        let mut bytes = [0u8; 4];
        for byte in &mut bytes {
            *byte = self.read_u8();
        }
        u32::from_be_bytes(bytes)
    }

    /// Returns true if the fw_cfg device is present.
    pub fn present(&self) -> bool {
        self.select(SIGNATURE);
        let mut signature = [0u8; 4];
        for byte in &mut signature {
            *byte = self.read_u8();
        }
        &signature == b"QEMU"
    }

    /// Looks up a named file in the fw_cfg file directory (for example
    /// `"etc/ramfb"`).
    pub fn find_file(&self, name: &str) -> Option<FileEntry> {
        if !self.present() {
            return None;
        }

        self.select(FILE_DIR);
        let count = self.read_be_u32();

        for _ in 0..count {
            let size = self.read_be_u32();
            let selector = self.read_be_u16();
            let _reserved = self.read_be_u16();

            let mut raw_name = [0u8; 56];
            for byte in &mut raw_name {
                *byte = self.read_u8();
            }
            let name_len = raw_name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(raw_name.len());

            if &raw_name[..name_len] == name.as_bytes() {
                return Some(FileEntry { size, selector });
            }
        }

        None
    }

    /// Writes `data` to the fw_cfg item `selector` through the DMA interface.
    ///
    /// The transfer is synchronous: QEMU completes it before the write to the
    /// low half of the DMA address register returns.
    pub fn dma_write(&self, selector: u16, data: &[u8]) -> bool {
        #[repr(C, align(8))]
        struct DmaAccess {
            control: u32,
            length: u32,
            address: u64,
        }

        let mut access = DmaAccess {
            control: (((selector as u32) << 16) | CONTROL_SELECT | CONTROL_WRITE).to_be(),
            length: (data.len() as u32).to_be(),
            address: (data.as_ptr() as u64).to_be(),
        };

        // SAFETY: the descriptor is a live local, and `base` is MMIO.
        unsafe {
            let descriptor = addr_of_mut!(access) as u64;
            // High half first, then the low half, which starts the transfer.
            write_volatile(
                (self.base + DMA_ADDRESS_HIGH) as *mut u32,
                ((descriptor >> 32) as u32).to_be(),
            );
            write_volatile(
                (self.base + DMA_ADDRESS_LOW) as *mut u32,
                (descriptor as u32).to_be(),
            );

            loop {
                let control = u32::from_be(read_volatile(addr_of!(access.control)));
                if control == 0 {
                    return true;
                }
                if control & CONTROL_ERROR != 0 {
                    return false;
                }
                core::hint::spin_loop();
            }
        }
    }
}
