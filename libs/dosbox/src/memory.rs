//! The PC address space's RAM.
//!
//! The first megabyte plus the high memory area is always backed. Extended
//! memory above that is allocated lazily in 64 KiB chunks, so a program that
//! only uses a little of it costs little of the host's heap. Unbacked addresses
//! read as zero, and a write that can't get a chunk is dropped, so a guest can't
//! escape the allocation or exhaust the heap through memory accesses alone.

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
/// Where extended memory starts.
pub const EXTENDED_BASE: u32 = 0x10_0000;
/// Extended memory reported to the guest, in KiB: 16 MiB of RAM in total.
pub const EXTENDED_KB: u32 = 15 * 1024;

/// The first megabyte and the high memory area.
const LOW_SIZE: u32 = 0x11_0000;
const CHUNK_SHIFT: u32 = 16;
const CHUNK_SIZE: usize = 1 << CHUNK_SHIFT;
const TOP: u32 = EXTENDED_BASE + EXTENDED_KB * 1024;

/// The guest's memory.
pub struct Memory {
    data: Vec<u8>,
    /// Chunks of extended memory above the high memory area.
    chunks: Vec<Option<Vec<u8>>>,
    /// The address mask: `A20` clear folds addresses onto the first megabyte.
    a20_mask: u32,
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

impl Memory {
    pub fn new() -> Self {
        let count = ((TOP - LOW_SIZE) >> CHUNK_SHIFT) as usize;
        let mut chunks = Vec::new();
        chunks.resize_with(count, || None);
        Self {
            data: vec![0; LOW_SIZE as usize],
            chunks,
            a20_mask: !(1 << 20),
        }
    }

    pub fn len(&self) -> usize {
        0x10_0000
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Sets the state of the A20 gate.
    pub fn set_a20(&mut self, on: bool) {
        self.a20_mask = if on { u32::MAX } else { !(1 << 20) };
    }

    pub fn a20(&self) -> bool {
        self.a20_mask == u32::MAX
    }

    /// The first megabyte.
    pub fn as_slice(&self) -> &[u8] {
        &self.data[..0x10_0000]
    }

    /// The first megabyte.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data[..0x10_0000]
    }

    #[inline(always)]
    pub fn read8(&self, addr: u32) -> u8 {
        let addr = addr & self.a20_mask;
        if addr < LOW_SIZE {
            self.data[addr as usize]
        } else {
            self.read_extended(addr)
        }
    }

    #[inline(always)]
    pub fn write8(&mut self, addr: u32, value: u8) {
        let addr = addr & self.a20_mask;
        if addr < LOW_SIZE {
            self.data[addr as usize] = value;
        } else {
            self.write_extended(addr, value);
        }
    }

    #[inline(never)]
    fn read_extended(&self, addr: u32) -> u8 {
        if addr >= TOP {
            return 0xFF;
        }
        let index = ((addr - LOW_SIZE) >> CHUNK_SHIFT) as usize;
        match &self.chunks[index] {
            Some(chunk) => chunk[(addr as usize) & (CHUNK_SIZE - 1)],
            None => 0,
        }
    }

    #[inline(never)]
    fn write_extended(&mut self, addr: u32, value: u8) {
        if addr >= TOP {
            return;
        }
        let index = ((addr - LOW_SIZE) >> CHUNK_SHIFT) as usize;
        let slot = &mut self.chunks[index];
        if slot.is_none() {
            let mut chunk = Vec::new();
            if chunk.try_reserve_exact(CHUNK_SIZE).is_err() {
                return;
            }
            chunk.resize(CHUNK_SIZE, 0);
            *slot = Some(chunk);
        }
        if let Some(chunk) = slot {
            chunk[(addr as usize) & (CHUNK_SIZE - 1)] = value;
        }
    }

    #[inline(always)]
    pub fn read16(&self, addr: u32) -> u16 {
        let at = (addr & self.a20_mask) as usize;
        if at + 1 < LOW_SIZE as usize {
            return u16::from_le_bytes([self.data[at], self.data[at + 1]]);
        }
        self.read8(addr) as u16 | ((self.read8(addr.wrapping_add(1)) as u16) << 8)
    }

    #[inline(always)]
    pub fn write16(&mut self, addr: u32, value: u16) {
        let at = (addr & self.a20_mask) as usize;
        if at + 1 < LOW_SIZE as usize {
            self.data[at..at + 2].copy_from_slice(&value.to_le_bytes());
            return;
        }
        self.write8(addr, value as u8);
        self.write8(addr.wrapping_add(1), (value >> 8) as u8);
    }

    #[inline(always)]
    pub fn read32(&self, addr: u32) -> u32 {
        let at = (addr & self.a20_mask) as usize;
        if at + 3 < LOW_SIZE as usize {
            return u32::from_le_bytes([
                self.data[at],
                self.data[at + 1],
                self.data[at + 2],
                self.data[at + 3],
            ]);
        }
        self.read16(addr) as u32 | ((self.read16(addr.wrapping_add(2)) as u32) << 16)
    }

    #[inline(always)]
    pub fn write32(&mut self, addr: u32, value: u32) {
        let at = (addr & self.a20_mask) as usize;
        if at + 3 < LOW_SIZE as usize {
            self.data[at..at + 4].copy_from_slice(&value.to_le_bytes());
            return;
        }
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

    /// Finds the contiguous host range for a guest range, if there is one and
    /// the guest range can be used directly. Returns `(chunk, offset)` with
    /// chunk `None` for low memory.
    fn plain_range(&self, addr: u32, len: u32) -> Option<(Option<usize>, usize)> {
        let end = addr.checked_add(len)?;
        if addr & self.a20_mask != addr || (end - 1) & self.a20_mask != end - 1 {
            return None;
        }
        if end <= 0xA_0000 {
            return Some((None, addr as usize));
        }
        if addr < LOW_SIZE || end > TOP {
            return None;
        }
        let index = ((addr - LOW_SIZE) >> CHUNK_SHIFT) as usize;
        if ((end - 1 - LOW_SIZE) >> CHUNK_SHIFT) as usize != index {
            return None;
        }
        self.chunks[index].as_ref()?;
        Some((Some(index), (addr as usize) & (CHUNK_SIZE - 1)))
    }

    /// Fills conventional or allocated extended RAM with a repeating pattern.
    /// Returns false when the range isn't plain contiguous RAM.
    pub fn fast_fill(&mut self, addr: u32, pattern: u32, width: u32, len: u32) -> bool {
        let Some((chunk, at)) = self.plain_range(addr, len) else {
            return false;
        };
        let slice = match chunk {
            None => &mut self.data[at..at + len as usize],
            Some(index) => match self.chunks[index].as_mut() {
                Some(data) => &mut data[at..at + len as usize],
                None => return false,
            },
        };
        match width {
            1 => slice.fill(pattern as u8),
            2 => {
                for cell in slice.chunks_exact_mut(2) {
                    cell.copy_from_slice(&(pattern as u16).to_le_bytes());
                }
            }
            _ => {
                for cell in slice.chunks_exact_mut(4) {
                    cell.copy_from_slice(&pattern.to_le_bytes());
                }
            }
        }
        true
    }

    /// Copies forward between plain RAM ranges, when that's the same as a
    /// byte-by-byte copy. Returns false to ask for the slow path.
    pub fn fast_copy(&mut self, dst: u32, src: u32, len: u32) -> bool {
        // An overlap with the destination ahead of the source propagates bytes.
        if dst > src && dst - src < len {
            return false;
        }
        let (Some((dst_chunk, dst_at)), Some((src_chunk, src_at))) =
            (self.plain_range(dst, len), self.plain_range(src, len))
        else {
            return false;
        };
        let len = len as usize;
        match (dst_chunk, src_chunk) {
            (None, None) => self.data.copy_within(src_at..src_at + len, dst_at),
            (Some(d), Some(s)) if d == s => {
                if let Some(chunk) = self.chunks[d].as_mut() {
                    chunk.copy_within(src_at..src_at + len, dst_at);
                }
            }
            (None, Some(s)) => {
                if let Some(chunk) = self.chunks[s].as_ref() {
                    self.data[dst_at..dst_at + len].copy_from_slice(&chunk[src_at..src_at + len]);
                }
            }
            (Some(d), None) => {
                if let Some(chunk) = self.chunks[d].as_mut() {
                    chunk[dst_at..dst_at + len].copy_from_slice(&self.data[src_at..src_at + len]);
                }
            }
            (Some(d), Some(s)) => {
                let source = self.chunks[s].take();
                if let (Some(from), Some(to)) = (source.as_ref(), self.chunks[d].as_mut()) {
                    to[dst_at..dst_at + len].copy_from_slice(&from[src_at..src_at + len]);
                }
                self.chunks[s] = source;
            }
        }
        true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a20_gate_folds_the_high_memory_area() {
        let mut mem = Memory::new();
        mem.write8(0x123, 0xAA);
        assert_eq!(mem.read8(0x10_0123), 0xAA);
        mem.set_a20(true);
        assert_eq!(mem.read8(0x10_0123), 0);
        mem.write8(0x10_0123, 0xBB);
        assert_eq!(mem.read8(0x123), 0xAA);
        assert_eq!(mem.read8(0x10_0123), 0xBB);
    }

    #[test]
    fn extended_memory_is_lazy_and_bounded() {
        let mut mem = Memory::new();
        mem.set_a20(true);
        assert_eq!(mem.read8(0x50_0000), 0);
        assert!(mem.chunks.iter().all(Option::is_none));
        mem.write32(0x50_FFFE, 0x1234_5678);
        assert_eq!(mem.read32(0x50_FFFE), 0x1234_5678);
        assert_eq!(mem.chunks.iter().flatten().count(), 2);
        mem.write8(0xFFFF_FFFF, 1);
        assert_eq!(mem.read8(0xFFFF_FFFF), 0xFF);
    }

    #[test]
    fn fast_copy_refuses_propagating_overlaps() {
        let mut mem = Memory::new();
        assert!(!mem.fast_copy(0x1001, 0x1000, 16));
        assert!(mem.fast_copy(0x1000, 0x1001, 16));
        assert!(!mem.fast_fill(0xA0000, 0, 1, 16));
    }
}
