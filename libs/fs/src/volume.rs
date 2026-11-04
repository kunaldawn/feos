//! Byte-addressed access on top of a [`BlockDevice`].
//!
//! Filesystem sectors are not necessarily the device's logical blocks: a real
//! disk usually exposes 512-byte blocks while a filesystem may use 4096-byte
//! sectors. This layer hides the difference and provides checked little-endian
//! scalar reads used by both on-disk format parsers.
//!
//! Metadata reads are small and clustered: walking a FAT asks for four bytes at
//! a time from the same sector. A small [`SectorCache`] serves those without
//! going back to the device. Whole-block reads bypass it.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::block::{BlockDevice, WritableBlockDevice, MAX_BLOCK_SIZE};
use crate::FsError;

/// Lines in the sector cache. Each holds one device block.
const CACHE_LINES: usize = 16;

/// A device block stored in atomics, so readers sharing `&self` can use it
/// without `unsafe`.
struct CacheLine {
    /// Held while the line is read or filled. It is only ever tried, never
    /// waited on, so a reader can't deadlock with one it interrupted.
    busy: AtomicBool,
    /// Block number plus one, or zero when empty.
    tag: AtomicU64,
    words: Vec<AtomicU64>,
}

impl CacheLine {
    fn store(&self, data: &[u8]) {
        for (word, chunk) in self.words.iter().zip(data.chunks(8)) {
            let mut bytes = [0u8; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            word.store(u64::from_le_bytes(bytes), Ordering::Relaxed);
        }
    }

    fn copy_to(&self, within: usize, out: &mut [u8]) {
        let mut done = 0usize;
        while done < out.len() {
            let position = within + done;
            let bytes = self.words[position / 8]
                .load(Ordering::Relaxed)
                .to_le_bytes();
            let start = position % 8;
            let take = (8 - start).min(out.len() - done);
            out[done..done + take].copy_from_slice(&bytes[start..start + take]);
            done += take;
        }
    }
}

/// A small, fully associative, write-through cache of device blocks.
///
/// Full associativity matters at this size: FSInfo and the two FAT copies are
/// touched together on every allocation, and would keep evicting each other if
/// they mapped to the same line.
///
/// A `Volume` owns its device and all writes go through it, so nothing can
/// change a cached block behind its back.
struct SectorCache {
    lines: Vec<CacheLine>,
    /// Round-robin replacement position.
    clock: AtomicUsize,
}

impl SectorCache {
    /// Returns `None` if the heap can't spare it; the volume then works uncached.
    fn new(block_size: usize) -> Option<Self> {
        let mut lines = Vec::new();
        lines.try_reserve_exact(CACHE_LINES).ok()?;
        for _ in 0..CACHE_LINES {
            let count = block_size.div_ceil(8);
            let mut words = Vec::new();
            words.try_reserve_exact(count).ok()?;
            words.resize_with(count, || AtomicU64::new(0));
            lines.push(CacheLine {
                busy: AtomicBool::new(false),
                tag: AtomicU64::new(0),
                words,
            });
        }
        Some(Self {
            lines,
            clock: AtomicUsize::new(0),
        })
    }

    fn victim(&self) -> &CacheLine {
        let index = self.clock.fetch_add(1, Ordering::Relaxed);
        &self.lines[index % CACHE_LINES]
    }

    /// Lines holding `block`. Two readers can briefly fill two lines with the
    /// same block, which is harmless because a write updates them all.
    fn holding(&self, block: u64) -> impl Iterator<Item = &CacheLine> {
        self.lines
            .iter()
            .filter(move |line| line.tag.load(Ordering::Relaxed) == block + 1)
    }

    /// Records a written block. `allocate` also installs it if it's absent.
    /// Writers hold `&mut Volume`, so no reader can be using a line.
    fn update(&self, block: u64, data: &[u8], allocate: bool) {
        let mut present = false;
        for line in self.holding(block) {
            line.store(data);
            present = true;
        }
        if allocate && !present {
            let line = self.victim();
            line.store(data);
            line.tag.store(block + 1, Ordering::Relaxed);
        }
    }

    fn invalidate(&self, block: u64) {
        for line in self.holding(block) {
            line.tag.store(0, Ordering::Relaxed);
        }
    }
}

impl CacheLine {
    /// Takes the line without waiting.
    fn try_acquire(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn release(&self) {
        self.busy.store(false, Ordering::Release);
    }
}

/// Runs `action` with a one-block buffer, without paying to clear
/// [`MAX_BLOCK_SIZE`] bytes for the usual 512-byte block.
fn with_block_buffer<R>(block_size: usize, action: impl FnOnce(&mut [u8]) -> R) -> R {
    if block_size <= 512 {
        let mut buffer = [0u8; 512];
        action(&mut buffer[..block_size])
    } else {
        let mut buffer = [0u8; MAX_BLOCK_SIZE];
        action(&mut buffer[..block_size])
    }
}

/// Filesystem formats this crate can mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeKind {
    /// FAT12/16/32 with a FAT32 directory layout (only FAT32 is accepted).
    Fat32,
    /// Extended File Allocation Table.
    Exfat,
}

pub(crate) struct Volume<D: BlockDevice> {
    device: D,
    block_size: usize,
    len: u64,
    cache: Option<SectorCache>,
}

impl<D: BlockDevice> Volume<D> {
    pub(crate) fn new(device: D) -> Result<Self, FsError> {
        let block_size = device.block_size();
        if block_size == 0 || block_size > MAX_BLOCK_SIZE || !block_size.is_power_of_two() {
            return Err(FsError::UnsupportedBlockSize);
        }
        let len = device
            .block_count()
            .checked_mul(block_size as u64)
            .ok_or(FsError::OutOfBounds)?;
        Ok(Self {
            device,
            block_size,
            len,
            cache: None,
        })
    }

    /// Like [`new`](Self::new), for a volume that will stay mounted.
    pub(crate) fn cached(device: D) -> Result<Self, FsError> {
        let mut volume = Self::new(device)?;
        volume.cache = SectorCache::new(volume.block_size);
        Ok(volume)
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Copies part of one block, through the cache when a line is free.
    fn read_partial(&self, block: u64, within: usize, out: &mut [u8]) -> Result<(), FsError> {
        if let Some(cache) = self.cache.as_ref() {
            for line in cache.holding(block) {
                if line.try_acquire() {
                    // The tag may have changed before the line was ours.
                    let hit = line.tag.load(Ordering::Relaxed) == block + 1;
                    if hit {
                        line.copy_to(within, out);
                    }
                    line.release();
                    if hit {
                        return Ok(());
                    }
                }
            }
        }

        with_block_buffer(self.block_size, |buffer| {
            self.device.read_block(block, buffer)?;
            out.copy_from_slice(&buffer[within..within + out.len()]);
            if let Some(cache) = self.cache.as_ref() {
                let line = cache.victim();
                if line.try_acquire() {
                    line.tag.store(0, Ordering::Relaxed);
                    line.store(buffer);
                    line.tag.store(block + 1, Ordering::Relaxed);
                    line.release();
                }
            }
            Ok(())
        })
    }

    /// Copies `out.len()` bytes starting at `offset` into `out`.
    pub(crate) fn read_bytes(&self, offset: u64, out: &mut [u8]) -> Result<(), FsError> {
        let end = offset
            .checked_add(out.len() as u64)
            .ok_or(FsError::OutOfBounds)?;
        if end > self.len {
            return Err(FsError::OutOfBounds);
        }

        let block_size = self.block_size as u64;
        let mut done = 0usize;
        while done < out.len() {
            let absolute = offset + done as u64;
            let block = absolute / block_size;
            let within = (absolute % block_size) as usize;
            let chunk = (out.len() - done).min(self.block_size - within);

            if within == 0 && chunk == self.block_size {
                self.device
                    .read_block(block, &mut out[done..done + chunk])?;
            } else {
                self.read_partial(block, within, &mut out[done..done + chunk])?;
            }
            done += chunk;
        }
        Ok(())
    }

    pub(crate) fn read_array<const N: usize>(&self, offset: u64) -> Result<[u8; N], FsError> {
        let mut out = [0u8; N];
        self.read_bytes(offset, &mut out)?;
        Ok(out)
    }

    pub(crate) fn read_u32_le(&self, offset: u64) -> Result<u32, FsError> {
        Ok(u32::from_le_bytes(self.read_array::<4>(offset)?))
    }

    pub(crate) fn read_u16_le(&self, offset: u64) -> Result<u16, FsError> {
        Ok(u16::from_le_bytes(self.read_array::<2>(offset)?))
    }
}

impl<D: WritableBlockDevice> Volume<D> {
    pub(crate) fn write_bytes(&mut self, offset: u64, data: &[u8]) -> Result<(), FsError> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(FsError::OutOfBounds)?;
        if end > self.len {
            return Err(FsError::OutOfBounds);
        }

        let block_size = self.block_size as u64;
        let mut done = 0usize;
        while done < data.len() {
            let absolute = offset + done as u64;
            let block = absolute / block_size;
            let within = (absolute % block_size) as usize;
            let chunk = (data.len() - done).min(self.block_size - within);

            if within == 0 && chunk == self.block_size {
                let source = &data[done..done + chunk];
                self.write_block_through(block, source, false)?;
            } else {
                let block_size = self.block_size;
                with_block_buffer(block_size, |buffer| {
                    self.read_partial(block, 0, buffer)?;
                    buffer[within..within + chunk].copy_from_slice(&data[done..done + chunk]);
                    self.write_block_through(block, buffer, true)
                })?;
            }
            done += chunk;
        }
        Ok(())
    }

    /// Writes a block and keeps the cache in step. After a failed write the
    /// device's contents are unknown, so the line is dropped.
    fn write_block_through(
        &mut self,
        block: u64,
        data: &[u8],
        allocate: bool,
    ) -> Result<(), FsError> {
        let result = self.device.write_block(block, data);
        if let Some(cache) = self.cache.as_ref() {
            match result {
                Ok(()) => cache.update(block, data, allocate),
                Err(_) => cache.invalidate(block),
            }
        }
        result
    }

    pub(crate) fn write_u16_le(&mut self, offset: u64, value: u16) -> Result<(), FsError> {
        self.write_bytes(offset, &value.to_le_bytes())
    }

    pub(crate) fn write_u32_le(&mut self, offset: u64, value: u32) -> Result<(), FsError> {
        self.write_bytes(offset, &value.to_le_bytes())
    }

    pub(crate) fn write_u64_le(&mut self, offset: u64, value: u64) -> Result<(), FsError> {
        self.write_bytes(offset, &value.to_le_bytes())
    }

    pub(crate) fn flush(&mut self) -> Result<(), FsError> {
        self.device.flush()
    }
}

/// Probes the first sector of `device` and identifies its filesystem.
pub fn detect<D: BlockDevice>(device: &D) -> Result<VolumeKind, FsError> {
    let volume = Volume::new(device)?;
    let mut boot = [0u8; 512];
    volume.read_bytes(0, &mut boot)?;

    if &boot[3..11] == b"EXFAT   " {
        return Ok(VolumeKind::Exfat);
    }

    let signature = u16::from_le_bytes([boot[510], boot[511]]);
    if signature == 0xaa55 && boot[0] != 0 {
        return Ok(VolumeKind::Fat32);
    }

    // exFAT keeps a backup boot region 12 sectors in. The sector size is in the
    // damaged main record, so try each legal one.
    for shift in 9..=12u32 {
        let mut name = [0u8; 8];
        if volume.read_bytes((12u64 << shift) + 3, &mut name).is_ok() && &name == b"EXFAT   " {
            return Ok(VolumeKind::Exfat);
        }
    }

    Err(FsError::UnsupportedVolume)
}
