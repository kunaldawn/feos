//! The 1 MiB PC address space.
//!
//! Only conventional memory is backed by RAM. Addresses above 1 MiB alias the
//! last megabyte, as on an 8086 with the A20 line disabled, so a stray access
//! cannot escape the allocation.

use alloc::vec;
use alloc::vec::Vec;

/// Base of the colour text buffer.
pub const VIDEO_BASE: u32 = 0x000B_8000;
/// Base of the monochrome text buffer.
pub const MONO_BASE: u32 = 0x000B_0000;
/// BIOS data area.
pub const BDA_BASE: u32 = 0x0000_0400;
/// Interrupt vector table.
pub const IVT_BASE: u32 = 0x0000_0000;
/// Conventional memory reported to DOS, in KiB.
pub const CONVENTIONAL_KB: u16 = 640;

/// The guest's memory.
pub struct Memory {
    data: Vec<u8>,
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

impl Memory {
    pub fn new() -> Self {
        Self {
            data: vec![0; 0x10_0000],
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
    }

    #[inline]
    pub fn read8(&self, addr: u32) -> u8 {
        self.data[(addr & 0xF_FFFF) as usize]
    }

    #[inline]
    pub fn write8(&mut self, addr: u32, value: u8) {
        self.data[(addr & 0xF_FFFF) as usize] = value;
    }

    #[inline]
    pub fn read16(&self, addr: u32) -> u16 {
        self.read8(addr) as u16 | ((self.read8(addr.wrapping_add(1)) as u16) << 8)
    }

    #[inline]
    pub fn write16(&mut self, addr: u32, value: u16) {
        self.write8(addr, value as u8);
        self.write8(addr.wrapping_add(1), (value >> 8) as u8);
    }

    pub fn read32(&self, addr: u32) -> u32 {
        self.read16(addr) as u32 | ((self.read16(addr.wrapping_add(2)) as u32) << 16)
    }

    pub fn write32(&mut self, addr: u32, value: u32) {
        self.write16(addr, value as u16);
        self.write16(addr.wrapping_add(2), (value >> 16) as u16);
    }

    pub fn fill(&mut self, addr: u32, len: usize, value: u8) {
        for offset in 0..len {
            self.write8(addr.wrapping_add(offset as u32), value);
        }
    }

    pub fn copy_from(&mut self, addr: u32, bytes: &[u8]) {
        for (offset, byte) in bytes.iter().enumerate() {
            self.write8(addr.wrapping_add(offset as u32), *byte);
        }
    }

    /// Copies at most `max` bytes of a NUL-terminated string.
    pub fn read_cstring(&self, segment: u16, offset: u16, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let base = ((segment as u32) << 4).wrapping_add(offset as u32);
        for index in 0..max {
            let byte = self.read8(base.wrapping_add(index as u32));
            if byte == 0 {
                break;
            }
            out.push(byte);
        }
        out
    }

    /// Copies at most `max` bytes of a `$`-terminated string.
    pub fn read_dollar_string(&self, segment: u16, offset: u16, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let base = ((segment as u32) << 4).wrapping_add(offset as u32);
        for index in 0..max {
            let byte = self.read8(base.wrapping_add(index as u32));
            if byte == b'$' {
                break;
            }
            out.push(byte);
        }
        out
    }

    pub fn write_cstring(&mut self, segment: u16, offset: u16, text: &[u8]) {
        let base = ((segment as u32) << 4).wrapping_add(offset as u32);
        for (index, byte) in text.iter().enumerate() {
            self.write8(base.wrapping_add(index as u32), *byte);
        }
        self.write8(base.wrapping_add(text.len() as u32), 0);
    }
}
