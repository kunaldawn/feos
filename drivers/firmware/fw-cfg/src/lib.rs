//! QEMU fw_cfg driver (MMIO + I/O port, DMA), limited to what the
//! framebuffer needs.
//!
//! AArch64's `virt` machine maps fw_cfg in MMIO at `0x0902_0000`:
//!
//! | offset | register                                  |
//! |--------|-------------------------------------------|
//! | 0x00   | data (8-bit accesses used here)           |
//! | 0x08   | selector (16-bit, big-endian)             |
//! | 0x10   | DMA address (64-bit, big-endian)          |
//!
//! x86 PCs expose the same device through I/O ports `0x510` (selector),
//! `0x511` (data) and `0x514`/`0x518` (DMA address high/low). Construction
//! chooses the backend, so `feos-driver-ramfb` stays transport-agnostic.
//!
//! The DMA transfer descriptor is big-endian in both cases. See the QEMU
//! fw_cfg specification for details.

#![no_std]

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

#[cfg(target_arch = "x86_64")]
use feos_port_io::{inb, outl, outw};

const DATA: usize = 0x00;
const SELECTOR: usize = 0x08;
const DMA_ADDRESS_HIGH: usize = 0x10;
const DMA_ADDRESS_LOW: usize = 0x14;

const SIGNATURE: u16 = 0x0000;
const FILE_DIR: u16 = 0x0019;

const CONTROL_ERROR: u32 = 1 << 0;
const CONTROL_SELECT: u32 = 1 << 3;
const CONTROL_WRITE: u32 = 1 << 4;

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Debug)]
enum Backend {
    Mmio(usize),
    Io(u16),
}

#[cfg(not(target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug)]
enum Backend {
    Mmio(usize),
}

/// An entry in the fw_cfg file directory.
#[derive(Clone, Copy, Debug)]
pub struct FileEntry {
    /// Size of the item in bytes.
    pub size: u32,
    /// fw_cfg selector used to read or write the item.
    pub selector: u16,
}

/// A QEMU fw_cfg device.
#[derive(Clone, Copy, Debug)]
pub struct FwCfg {
    backend: Backend,
}

impl FwCfg {
    /// Creates a driver for a device mapped in MMIO at `base`.
    pub const fn new(base: usize) -> Self {
        Self {
            backend: Backend::Mmio(base),
        }
    }

    /// Creates a driver for a device behind I/O ports starting at `base`.
    ///
    /// On a PC `base` is `0x510` (the selector port); data and DMA ports are
    /// at fixed offsets, matching QEMU's `fw_cfg_init_io_dma`.
    #[cfg(target_arch = "x86_64")]
    pub const fn new_io(base: u16) -> Self {
        Self {
            backend: Backend::Io(base),
        }
    }

    /// Base address or I/O port of the device.
    pub const fn base(&self) -> usize {
        match self.backend {
            Backend::Mmio(base) => base,
            #[cfg(target_arch = "x86_64")]
            Backend::Io(base) => base as usize,
        }
    }

    fn select(&self, key: u16) {
        match self.backend {
            Backend::Mmio(base) => {
                // SAFETY: `base` is an MMIO address supplied by the board.
                unsafe { write_volatile((base + SELECTOR) as *mut u16, key.to_be()) };
            }
            #[cfg(target_arch = "x86_64")]
            Backend::Io(base) => {
                // The combined selector/data I/O region is little-endian.
                // SAFETY: `base` is a valid fw_cfg selector port.
                unsafe { outw(base, key) };
            }
        }
    }

    fn read_u8(&self) -> u8 {
        match self.backend {
            Backend::Mmio(base) => {
                // SAFETY: `base` is an MMIO address supplied by the board.
                unsafe { read_volatile((base + DATA) as *const u8) }
            }
            #[cfg(target_arch = "x86_64")]
            Backend::Io(base) => {
                // Data is one byte above the selector.
                // SAFETY: `base + 1` is the fw_cfg data port.
                unsafe { inb(base + 1) }
            }
        }
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

        // The device reads the descriptor and the data when kicked, so both
        // must be complete in memory first.
        feos_hal::dma::wmb();

        // SAFETY: the descriptor is a live local, and the device is either
        // MMIO or a fixed I/O port range supplied by the board.
        unsafe {
            let descriptor = addr_of_mut!(access) as u64;
            let high = ((descriptor >> 32) as u32).to_be();
            let low = (descriptor as u32).to_be();

            match self.backend {
                Backend::Mmio(base) => {
                    // High half first, then the low half, which starts it.
                    write_volatile((base + DMA_ADDRESS_HIGH) as *mut u32, high);
                    write_volatile((base + DMA_ADDRESS_LOW) as *mut u32, low);
                }
                #[cfg(target_arch = "x86_64")]
                Backend::Io(base) => {
                    outl(base + 4, high);
                    outl(base + 8, low);
                }
            }

            loop {
                // The device writes the status through DMA.
                let control = u32::from_be(read_volatile(addr_of!(access.control)));
                if control == 0 {
                    fence(Ordering::Acquire);
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
