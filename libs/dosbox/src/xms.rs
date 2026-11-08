//! The extended and expanded memory drivers: XMS 3.0 through a far-call entry
//! and LIM EMS 4.0 through `INT 67h`, both living on the extended RAM.

use alloc::vec;
use alloc::vec::Vec;

use crate::cpu::CpuRegs;
use crate::dos::guest_write;
use crate::memory::{Memory, EXTENDED_BASE, EXTENDED_KB};
use crate::vga::Vga;

/// Where allocatable XMS starts: just above the high memory area, in KiB.
const XMS_BASE_KB: u32 = 1024 + 64;
/// The expanded memory pool, carved off the top of extended memory.
const EMS_PAGES: usize = 256;
const EMS_PAGE_SIZE: u32 = 16 * 1024;
const EMS_POOL_BASE: u32 = EXTENDED_BASE + EXTENDED_KB * 1024 - EMS_PAGES as u32 * EMS_PAGE_SIZE;
/// The EMS page frame segment.
pub const EMS_FRAME: u32 = 0xD_0000;
const XMS_TOP_KB: u32 = EMS_POOL_BASE / 1024;
const MAX_XMS_HANDLES: usize = 512;
const MAX_EMS_HANDLES: usize = 32;

#[derive(Clone, Copy)]
struct XmsBlock {
    start_kb: u32,
    size_kb: u32,
    locks: u8,
}

/// The XMS driver.
pub struct Xms {
    blocks: Vec<Option<XmsBlock>>,
    hma_taken: bool,
    /// Local A20 enable count.
    a20_local: u32,
}

impl Default for Xms {
    fn default() -> Self {
        Self::new()
    }
}

impl Xms {
    pub fn new() -> Self {
        Self {
            blocks: vec![None; MAX_XMS_HANDLES],
            hma_taken: false,
            a20_local: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    fn largest_and_total(&self) -> (u32, u32) {
        let mut used: Vec<(u32, u32)> = self
            .blocks
            .iter()
            .flatten()
            .map(|block| (block.start_kb, block.size_kb))
            .collect();
        used.sort_unstable();
        let mut cursor = XMS_BASE_KB;
        let mut largest = 0;
        let mut total = 0;
        for (start, size) in used {
            let gap = start.saturating_sub(cursor);
            largest = largest.max(gap);
            total += gap;
            cursor = cursor.max(start + size);
        }
        let gap = XMS_TOP_KB.saturating_sub(cursor);
        largest = largest.max(gap);
        total += gap;
        (largest, total)
    }

    fn find_gap(&self, size_kb: u32) -> Option<u32> {
        let mut used: Vec<(u32, u32)> = self
            .blocks
            .iter()
            .flatten()
            .map(|block| (block.start_kb, block.size_kb))
            .collect();
        used.sort_unstable();
        let mut cursor = XMS_BASE_KB;
        for (start, size) in used {
            if start.saturating_sub(cursor) >= size_kb {
                return Some(cursor);
            }
            cursor = cursor.max(start + size);
        }
        (XMS_TOP_KB.saturating_sub(cursor) >= size_kb).then_some(cursor)
    }

    /// The largest free block and the total free, in KiB.
    pub fn free_kb(&self) -> (u32, u32) {
        self.largest_and_total()
    }

    /// The largest free block, in KiB.
    pub fn largest_free_kb(&self) -> u32 {
        self.largest_and_total().0
    }

    /// Takes a block for the machine's own use: its handle and physical address.
    pub fn alloc_block(&mut self, size_kb: u32) -> Option<(u16, u32)> {
        let slot = self.blocks.iter().position(Option::is_none)?;
        let start_kb = self.find_gap(size_kb)?;
        self.blocks[slot] = Some(XmsBlock {
            start_kb,
            size_kb,
            locks: 0,
        });
        Some((slot as u16 + 1, start_kb * 1024))
    }

    /// Takes a block at a given address, if that's free.
    pub fn alloc_block_at(&mut self, address: u32, size_kb: u32) -> Option<(u16, u32)> {
        if !address.is_multiple_of(1024) {
            return None;
        }
        let start_kb = address / 1024;
        let free = start_kb >= XMS_BASE_KB
            && start_kb.checked_add(size_kb)? <= XMS_TOP_KB
            && self.blocks.iter().flatten().all(|other| {
                other.start_kb + other.size_kb <= start_kb
                    || other.start_kb >= start_kb + size_kb
            });
        if !free {
            return None;
        }
        let slot = self.blocks.iter().position(Option::is_none)?;
        self.blocks[slot] = Some(XmsBlock {
            start_kb,
            size_kb,
            locks: 0,
        });
        Some((slot as u16 + 1, address))
    }

    /// Gives a block taken with [`Self::alloc_block`] back.
    pub fn free_block(&mut self, handle: u16) {
        if let Some(index) = self.handle(handle) {
            self.blocks[index] = None;
        }
    }

    /// Resizes a block, moving it if it must: its new address.
    pub fn resize_block(&mut self, handle: u16, size_kb: u32, mem: &mut Memory) -> Option<u32> {
        let index = self.handle(handle)?;
        let block = self.blocks[index]?;
        self.blocks[index] = None;
        let same_place = self.blocks.iter().flatten().all(|other| {
            other.start_kb + other.size_kb <= block.start_kb
                || other.start_kb >= block.start_kb + size_kb
        }) && block.start_kb + size_kb <= XMS_TOP_KB;
        let start_kb = if same_place {
            Some(block.start_kb)
        } else {
            self.find_gap(size_kb)
        };
        let Some(start_kb) = start_kb else {
            self.blocks[index] = Some(block);
            return None;
        };
        if start_kb != block.start_kb {
            for offset in 0..block.size_kb.min(size_kb) * 1024 {
                let byte = mem.read8(block.start_kb * 1024 + offset);
                mem.write8(start_kb * 1024 + offset, byte);
            }
        }
        self.blocks[index] = Some(XmsBlock {
            start_kb,
            size_kb,
            locks: 0,
        });
        Some(start_kb * 1024)
    }

    fn handle(&self, handle: u16) -> Option<usize> {
        let index = (handle as usize).checked_sub(1)?;
        self.blocks.get(index)?.as_ref().map(|_| index)
    }

    /// One far call to the driver. Returns the bytes moved, for the cost.
    pub fn call(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, vga: &mut Vga) -> u32 {
        let done = |regs: &mut CpuRegs<'_>| {
            regs.set_ax(1);
            regs.set_bl(0);
        };
        let failed = |regs: &mut CpuRegs<'_>, code: u8| {
            regs.set_ax(0);
            regs.set_bl(code);
        };
        match regs.ah() {
            0x00 => {
                regs.set_ax(0x0300);
                regs.set_bx(0x0301);
                regs.set_dx(1);
            }
            0x01 => {
                if self.hma_taken {
                    failed(regs, 0x91);
                } else {
                    self.hma_taken = true;
                    done(regs);
                }
            }
            0x02 => {
                if self.hma_taken {
                    self.hma_taken = false;
                    done(regs);
                } else {
                    failed(regs, 0x93);
                }
            }
            0x03 => {
                mem.set_a20(true);
                done(regs);
            }
            0x04 => {
                if self.a20_local == 0 {
                    mem.set_a20(false);
                }
                done(regs);
            }
            0x05 => {
                self.a20_local = self.a20_local.saturating_add(1);
                mem.set_a20(true);
                done(regs);
            }
            0x06 => {
                self.a20_local = self.a20_local.saturating_sub(1);
                if self.a20_local == 0 {
                    mem.set_a20(false);
                }
                done(regs);
            }
            0x07 => {
                regs.set_ax(u16::from(mem.a20()));
                regs.set_bl(0);
            }
            0x08 => {
                let (largest, total) = self.largest_and_total();
                regs.set_ax(largest.min(0xFFFF) as u16);
                regs.set_dx(total.min(0xFFFF) as u16);
                regs.set_bl(0);
            }
            0x09 => {
                let size = regs.dx() as u32;
                let Some(slot) = self.blocks.iter().position(Option::is_none) else {
                    failed(regs, 0xA1);
                    return 0;
                };
                let Some(start_kb) = self.find_gap(size) else {
                    failed(regs, 0xA0);
                    return 0;
                };
                self.blocks[slot] = Some(XmsBlock {
                    start_kb,
                    size_kb: size,
                    locks: 0,
                });
                regs.set_dx(slot as u16 + 1);
                done(regs);
            }
            0x0A => match self.handle(regs.dx()) {
                Some(index) => {
                    if self.blocks[index].is_some_and(|block| block.locks != 0) {
                        failed(regs, 0xAB);
                    } else {
                        self.blocks[index] = None;
                        done(regs);
                    }
                }
                None => failed(regs, 0xA2),
            },
            0x0B => return self.block_move(regs, mem, vga),
            0x0C => match self.handle(regs.dx()) {
                Some(index) => {
                    if let Some(block) = self.blocks[index].as_mut() {
                        block.locks = block.locks.saturating_add(1);
                        let address = block.start_kb * 1024;
                        regs.set_dx((address >> 16) as u16);
                        regs.set_bx(address as u16);
                        regs.set_ax(1);
                    }
                }
                None => failed(regs, 0xA2),
            },
            0x0D => match self.handle(regs.dx()) {
                Some(index) => {
                    if let Some(block) = self.blocks[index].as_mut() {
                        if block.locks == 0 {
                            failed(regs, 0xAA);
                        } else {
                            block.locks -= 1;
                            done(regs);
                        }
                    }
                }
                None => failed(regs, 0xA2),
            },
            0x0E => match self.handle(regs.dx()) {
                Some(index) => {
                    let block = self.blocks[index];
                    let free = self.blocks.iter().filter(|slot| slot.is_none()).count();
                    regs.set_ax(1);
                    regs.set_bh(block.map_or(0, |block| block.locks));
                    regs.set_bl(free.min(255) as u8);
                    regs.set_dx(block.map_or(0, |block| block.size_kb.min(0xFFFF) as u16));
                }
                None => failed(regs, 0xA2),
            },
            0x0F => match self.handle(regs.dx()) {
                Some(index) => {
                    let Some(block) = self.blocks[index] else {
                        failed(regs, 0xA2);
                        return 0;
                    };
                    let new_size = regs.bx() as u32;
                    if block.locks != 0 {
                        failed(regs, 0xAB);
                        return 0;
                    }
                    // Resize in place when it fits, otherwise move elsewhere.
                    self.blocks[index] = None;
                    let same_place = self.blocks.iter().flatten().all(|other| {
                        other.start_kb + other.size_kb <= block.start_kb
                            || other.start_kb >= block.start_kb + new_size
                    }) && block.start_kb + new_size <= XMS_TOP_KB;
                    let start_kb = if same_place {
                        Some(block.start_kb)
                    } else {
                        self.find_gap(new_size)
                    };
                    match start_kb {
                        Some(start_kb) => {
                            let copied = block.size_kb.min(new_size) * 1024;
                            if start_kb != block.start_kb {
                                for offset in 0..copied {
                                    let byte = mem.read8(block.start_kb * 1024 + offset);
                                    mem.write8(start_kb * 1024 + offset, byte);
                                }
                            }
                            self.blocks[index] = Some(XmsBlock {
                                start_kb,
                                size_kb: new_size,
                                locks: 0,
                            });
                            done(regs);
                        }
                        None => {
                            self.blocks[index] = Some(block);
                            failed(regs, 0xA0);
                        }
                    }
                }
                None => failed(regs, 0xA2),
            },
            0x10 | 0x11 => {
                regs.set_ax(0);
                regs.set_bl(0xB1);
                regs.set_dx(0);
            }
            _ => failed(regs, 0x80),
        }
        0
    }

    fn resolve_move(&self, handle: u16, offset: u32, length: u32) -> Result<u32, u8> {
        if handle == 0 {
            // A real-mode far pointer.
            let address = ((offset >> 16) << 4) + (offset & 0xFFFF);
            return Ok(address);
        }
        let index = self.handle(handle).ok_or(0xA3u8)?;
        let block = self.blocks[index].ok_or(0xA3u8)?;
        let size = block.size_kb * 1024;
        if length > size || offset > size - length {
            return Err(0xA4);
        }
        Ok(block.start_kb * 1024 + offset)
    }

    fn block_move(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, vga: &mut Vga) -> u32 {
        let at = (((regs.ds() as u32) << 4) + regs.si() as u32) & 0xF_FFFF;
        let length = mem.read32(at);
        let source_handle = mem.read16(at + 4);
        let source_offset = mem.read32(at + 6);
        let dest_handle = mem.read16(at + 10);
        let dest_offset = mem.read32(at + 12);
        if length & 1 != 0 {
            regs.set_ax(0);
            regs.set_bl(0xA7);
            return 0;
        }
        let source = self.resolve_move(source_handle, source_offset, length);
        let dest = self.resolve_move(dest_handle, dest_offset, length);
        let (source, dest) = match (source, dest) {
            (Ok(source), Ok(dest)) => (source, dest),
            (Err(code), _) | (_, Err(code)) => {
                regs.set_ax(0);
                regs.set_bl(code);
                return 0;
            }
        };
        let length = length.min(XMS_TOP_KB * 1024);
        // The driver moves with A20 on, whatever its state.
        let a20 = mem.a20();
        mem.set_a20(true);
        if dest > source && dest < source.wrapping_add(length) {
            for offset in (0..length).rev() {
                let byte = crate::dos::guest_read(mem, vga, source.wrapping_add(offset));
                guest_write(mem, vga, dest.wrapping_add(offset), byte);
            }
        } else if !mem.fast_copy(dest, source, length) {
            for offset in 0..length {
                let byte = crate::dos::guest_read(mem, vga, source.wrapping_add(offset));
                guest_write(mem, vga, dest.wrapping_add(offset), byte);
            }
        }
        mem.set_a20(a20);
        regs.set_ax(1);
        regs.set_bl(0);
        length
    }

    /// `INT 15h AH=87h`: the BIOS block move. Returns the bytes moved.
    pub fn bios_move(regs: &mut CpuRegs<'_>, mem: &mut Memory, vga: &mut Vga) -> u32 {
        let table = (((regs.es() as u32) << 4) + regs.si() as u32) & 0xF_FFFF;
        let words = regs.cx() as u32;
        if words > 0x8000 {
            regs.set_ah(1);
            regs.set_carry(true);
            return 0;
        }
        let base = |mem: &Memory, at: u32| {
            mem.read16(at + 2) as u32
                | ((mem.read8(at + 4) as u32) << 16)
                | ((mem.read8(at + 7) as u32) << 24)
        };
        let source = base(mem, table + 0x10);
        let dest = base(mem, table + 0x18);
        let a20 = mem.a20();
        mem.set_a20(true);
        for offset in 0..words * 2 {
            let byte = crate::dos::guest_read(mem, vga, source.wrapping_add(offset));
            guest_write(mem, vga, dest.wrapping_add(offset), byte);
        }
        mem.set_a20(a20);
        regs.set_ah(0);
        regs.set_carry(false);
        words * 2
    }
}

/// The EMS driver.
pub struct Ems {
    /// Logical pages per handle, as indices into the pool. `None` is unused.
    handles: Vec<Option<Vec<u16>>>,
    used: Vec<bool>,
    /// The four physical pages' current mapping, as pool indices.
    map: [Option<u16>; 4],
    saved_maps: Vec<(u16, [Option<u16>; 4])>,
}

impl Default for Ems {
    fn default() -> Self {
        Self::new()
    }
}

impl Ems {
    pub fn new() -> Self {
        Self {
            handles: vec![None; MAX_EMS_HANDLES],
            used: vec![false; EMS_PAGES],
            map: [None; 4],
            saved_maps: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Where an address in the page frame goes.
    #[inline]
    /// Where the four pages of the frame are mapped now.
    pub fn frame_map(&self) -> [Option<u32>; 4] {
        self.map
            .map(|page| page.map(|page| EMS_POOL_BASE + page as u32 * EMS_PAGE_SIZE))
    }

    pub fn translate(&self, addr: u32) -> Option<u32> {
        let offset = addr.wrapping_sub(EMS_FRAME);
        if offset >= 0x1_0000 {
            return None;
        }
        let page = self.map[(offset / EMS_PAGE_SIZE) as usize]?;
        Some(EMS_POOL_BASE + page as u32 * EMS_PAGE_SIZE + offset % EMS_PAGE_SIZE)
    }

    fn free_pages(&self) -> u16 {
        self.used.iter().filter(|used| !**used).count() as u16
    }

    fn release(&mut self, index: usize) {
        if let Some(pages) = self.handles[index].take() {
            for page in pages {
                self.used[page as usize] = false;
                for slot in self.map.iter_mut() {
                    if *slot == Some(page) {
                        *slot = None;
                    }
                }
            }
        }
    }

    /// `INT 67h`.
    pub fn int67(&mut self, regs: &mut CpuRegs<'_>, mem: &mut Memory, vga: &mut Vga) {
        let handle = regs.dx() as usize;
        let valid = self.handles.get(handle).is_some_and(Option::is_some);
        match regs.ah() {
            0x40 => regs.set_ah(0),
            0x41 => {
                regs.set_bx((EMS_FRAME >> 4) as u16);
                regs.set_ah(0);
            }
            0x42 => {
                regs.set_bx(self.free_pages());
                regs.set_dx(EMS_PAGES as u16);
                regs.set_ah(0);
            }
            0x43 => {
                let count = regs.bx() as usize;
                if count == 0 {
                    regs.set_ah(0x89);
                } else if count > EMS_PAGES {
                    regs.set_ah(0x87);
                } else if count > self.free_pages() as usize {
                    regs.set_ah(0x88);
                } else if let Some(slot) = self.handles.iter().position(Option::is_none) {
                    let mut pages = Vec::new();
                    for (index, used) in self.used.iter_mut().enumerate() {
                        if pages.len() == count {
                            break;
                        }
                        if !*used {
                            *used = true;
                            pages.push(index as u16);
                        }
                    }
                    self.handles[slot] = Some(pages);
                    regs.set_dx(slot as u16);
                    regs.set_ah(0);
                } else {
                    regs.set_ah(0x85);
                }
            }
            0x44 => {
                let physical = regs.al() as usize;
                let logical = regs.bx();
                if !valid {
                    regs.set_ah(0x83);
                } else if physical >= 4 {
                    regs.set_ah(0x8B);
                } else if logical == 0xFFFF {
                    self.map[physical] = None;
                    regs.set_ah(0);
                } else {
                    let page = self.handles[handle]
                        .as_ref()
                        .and_then(|pages| pages.get(logical as usize).copied());
                    match page {
                        Some(page) => {
                            self.map[physical] = Some(page);
                            regs.set_ah(0);
                        }
                        None => regs.set_ah(0x8A),
                    }
                }
            }
            0x45 => {
                if !valid {
                    regs.set_ah(0x83);
                } else {
                    self.release(handle);
                    regs.set_ah(0);
                }
            }
            0x46 => {
                regs.set_al(0x40);
                regs.set_ah(0);
            }
            0x47 => {
                if self.saved_maps.len() >= MAX_EMS_HANDLES {
                    regs.set_ah(0x8C);
                } else if self
                    .saved_maps
                    .iter()
                    .any(|(saved, _)| *saved as usize == handle)
                {
                    regs.set_ah(0x8D);
                } else {
                    self.saved_maps.push((handle as u16, self.map));
                    regs.set_ah(0);
                }
            }
            0x48 => {
                match self
                    .saved_maps
                    .iter()
                    .position(|(saved, _)| *saved as usize == handle)
                {
                    Some(index) => {
                        self.map = self.saved_maps.swap_remove(index).1;
                        regs.set_ah(0);
                    }
                    None => regs.set_ah(0x8E),
                }
            }
            0x4B => {
                regs.set_bx(self.handles.iter().flatten().count() as u16);
                regs.set_ah(0);
            }
            0x4C => {
                if valid {
                    let count = self.handles[handle].as_ref().map_or(0, Vec::len);
                    regs.set_bx(count as u16);
                    regs.set_ah(0);
                } else {
                    regs.set_ah(0x83);
                }
            }
            0x4D => {
                let at = ((regs.es() as u32) << 4) + regs.di() as u32;
                let mut count = 0u16;
                for (index, pages) in self.handles.iter().enumerate() {
                    if let Some(pages) = pages {
                        let entry = at + count as u32 * 4;
                        for (offset, byte) in (index as u16)
                            .to_le_bytes()
                            .iter()
                            .chain((pages.len() as u16).to_le_bytes().iter())
                            .enumerate()
                        {
                            guest_write(mem, vga, entry + offset as u32, *byte);
                        }
                        count += 1;
                    }
                }
                regs.set_bx(count);
                regs.set_ah(0);
            }
            _ => regs.set_ah(0x84),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_regs<T>(
        ax: u16,
        bx: u16,
        dx: u16,
        run: impl FnOnce(&mut CpuRegs<'_>) -> T,
    ) -> ([u32; 8], T) {
        let mut r = [0u32; 8];
        let mut sregs = [0u16; 6];
        let mut flags = 0u32;
        r[0] = ax as u32;
        r[3] = bx as u32;
        r[2] = dx as u32;
        let mut regs = CpuRegs {
            r: &mut r,
            sregs: &mut sregs,
            flags: &mut flags,
        };
        let value = run(&mut regs);
        (r, value)
    }

    #[test]
    fn xms_allocates_locks_and_frees() {
        let mut xms = Xms::new();
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        let (r, _) = with_regs(0x0900, 0, 1024, |regs| xms.call(regs, &mut mem, &mut vga));
        assert_eq!(r[0] & 0xFFFF, 1);
        let handle = r[2] as u16;
        let (r, _) = with_regs(0x0C00, 0, handle, |regs| xms.call(regs, &mut mem, &mut vga));
        assert_eq!(r[0] & 0xFFFF, 1);
        assert_eq!(
            ((r[2] & 0xFFFF) << 16) | (r[3] & 0xFFFF),
            XMS_BASE_KB * 1024
        );
        let (r, _) = with_regs(0x0A00, 0, handle, |regs| xms.call(regs, &mut mem, &mut vga));
        assert_eq!(r[0] & 0xFFFF, 0, "locked blocks can't be freed");
        with_regs(0x0D00, 0, handle, |regs| xms.call(regs, &mut mem, &mut vga));
        let (r, _) = with_regs(0x0A00, 0, handle, |regs| xms.call(regs, &mut mem, &mut vga));
        assert_eq!(r[0] & 0xFFFF, 1);
    }

    #[test]
    fn ems_maps_pages_into_the_frame() {
        let mut ems = Ems::new();
        let mut mem = Memory::new();
        let mut vga = Vga::new();
        let (r, _) = with_regs(0x4300, 2, 0, |regs| ems.int67(regs, &mut mem, &mut vga));
        assert_eq!((r[0] >> 8) & 0xFF, 0);
        let handle = r[2] as u16;
        with_regs(0x4400, 1, handle, |regs| {
            ems.int67(regs, &mut mem, &mut vga)
        });
        assert_eq!(
            ems.translate(0xD_0005),
            Some(EMS_POOL_BASE + EMS_PAGE_SIZE + 5)
        );
        assert_eq!(ems.translate(0xD_4000), None);
        let (r, _) = with_regs(0x4400, 9, handle, |regs| {
            ems.int67(regs, &mut mem, &mut vga)
        });
        assert_eq!((r[0] >> 8) & 0xFF, 0x8A);
        with_regs(0x4500, 0, handle, |regs| {
            ems.int67(regs, &mut mem, &mut vga)
        });
        assert_eq!(ems.translate(0xD_0005), None);
    }
}
