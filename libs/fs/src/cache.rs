//! A write-back buffer cache, sitting between a filesystem and its disk.
//!
//! Reads share `&self`, so the state is behind a blocking mutex. Lines are 4 KiB
//! of consecutive blocks, big enough that a FAT or directory walk mostly hits.
//! Large transfers skip the cache: copying a video through it would only evict
//! everything useful.
//!
//! ## Ordering
//!
//! A filesystem survives a crash if things exist before anything refers to them.
//! Each dirty block carries a [`WriteClass`], and
//! [`commit`](WritableBlockDevice::commit) writes them in class order, with a
//! device flush between classes. It's per block, not per line: with small clusters
//! a line can hold a directory's entries beside a new directory's zeroed cluster,
//! and the second has to land before anything refers to it. Blocks can leave early
//! only when that keeps the order: [`Data`](WriteClass::Data) can be written at
//! any moment because nothing depends on it being absent; anything else forces a
//! commit. This is what lets a file's many small updates merge into
//! one write, and still never leaves a directory entry ahead of its chain.

use alloc::vec::Vec;

use feos_sync::Mutex;

use crate::block::{BlockDevice, WritableBlockDevice, WriteClass};
use crate::FsError;

/// Bytes per line, or one block when blocks are larger.
const LINE_BYTES: usize = 4096;
/// A transfer of at least this many whole lines goes straight to the device.
const BYPASS_LINES: u64 = 8;
const NO_LINE: u32 = u32::MAX;
/// Fewest lines the cache works with. Two FAT mirrors, a directory and FSInfo
/// must fit, or every metadata write forces a commit.
const MIN_LINES: usize = 16;

/// The most blocks a line can hold, at the smallest legal block size.
const MAX_BLOCKS_PER_LINE: usize = LINE_BYTES / 512;

const CLASSES: [WriteClass; 3] = [WriteClass::Data, WriteClass::Fat, WriteClass::Dir];

fn class_index(class: WriteClass) -> usize {
    match class {
        WriteClass::Data => 0,
        WriteClass::Fat => 1,
        WriteClass::Dir => 2,
    }
}

#[derive(Clone, Copy)]
struct Line {
    /// Line number plus one, or zero when empty.
    tag: u64,
    /// Set by each use, cleared by the clock hand.
    referenced: bool,
    /// Which of the line's blocks are dirty, and with what.
    dirty: [Option<WriteClass>; MAX_BLOCKS_PER_LINE],
    /// Next line in this hash bucket.
    chain: u32,
}

/// Counts of what a cache has done, for tests and tuning.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct CacheStats {
    /// Reads served from memory.
    pub read_hits: u64,
    /// Lines read from the device.
    pub read_misses: u64,
    /// Write requests sent to the device.
    pub device_writes: u64,
    /// Device flushes.
    pub flushes: u64,
    /// Commits that had to run because the cache was full of dirty lines.
    pub forced_commits: u64,
}

struct Inner<D> {
    device: D,
    block_size: usize,
    blocks_per_line: u64,
    block_count: u64,
    lines: Vec<Line>,
    /// `lines.len() * line_bytes` bytes of block data.
    data: Vec<u8>,
    buckets: Vec<u32>,
    hand: usize,
    line_bytes: usize,
    dirty: [usize; 3],
    stats: CacheStats,
    /// Set by a failed flush. The device may have dropped anything it had
    /// accepted, so a later commit must not report success on top of it.
    unflushed: bool,
}

/// A [`BlockDevice`] with a write-back cache in front. See the module docs.
pub struct BufferCache<D> {
    inner: Mutex<Inner<D>>,
    block_size: usize,
    block_count: u64,
    read_only: bool,
}

impl<D: BlockDevice> BufferCache<D> {
    /// Wraps `device` with about `capacity_bytes` of cache, fewer if the heap
    /// can't spare it. Fails only when even the minimum won't fit.
    pub fn new(device: D, capacity_bytes: usize) -> Result<Self, FsError> {
        let block_size = device.block_size();
        if block_size == 0 || !block_size.is_power_of_two() {
            return Err(FsError::UnsupportedBlockSize);
        }
        let line_bytes = LINE_BYTES.max(block_size);
        let mut count = (capacity_bytes / line_bytes).max(MIN_LINES);
        let mut data = Vec::new();
        while data.try_reserve_exact(count * line_bytes).is_err() {
            if count == MIN_LINES {
                return Err(FsError::OutOfMemory);
            }
            count = (count / 2).max(MIN_LINES);
        }
        data.resize(count * line_bytes, 0);

        let mut lines = Vec::new();
        lines
            .try_reserve_exact(count)
            .map_err(|_| FsError::OutOfMemory)?;
        lines.resize(
            count,
            Line {
                tag: 0,
                referenced: false,
                dirty: [None; MAX_BLOCKS_PER_LINE],
                chain: NO_LINE,
            },
        );
        let mut buckets = Vec::new();
        let bucket_count = (count * 2).next_power_of_two();
        buckets
            .try_reserve_exact(bucket_count)
            .map_err(|_| FsError::OutOfMemory)?;
        buckets.resize(bucket_count, NO_LINE);

        let block_count = device.block_count();
        let read_only = device.is_read_only();
        Ok(Self {
            inner: Mutex::new(Inner {
                device,
                block_size,
                blocks_per_line: (line_bytes / block_size) as u64,
                block_count,
                lines,
                data,
                buckets,
                hand: 0,
                line_bytes,
                dirty: [0; 3],
                stats: CacheStats::default(),
                unflushed: false,
            }),
            block_size,
            block_count,
            read_only,
        })
    }

    /// What the cache has done so far.
    pub fn stats(&self) -> CacheStats {
        self.inner.lock().stats
    }

    /// Dirty blocks waiting to be written.
    pub fn dirty_blocks(&self) -> usize {
        self.inner.lock().dirty.iter().sum()
    }

    /// Consumes the cache, returning the device. Dirty lines are lost; commit first.
    pub fn into_inner(self) -> D {
        self.inner.into_inner().device
    }
}

impl<D: BlockDevice> Inner<D> {
    fn bucket(&self, tag: u64) -> usize {
        // Multiplicative hash: neighbouring lines land far apart.
        let mixed = tag.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (mixed >> 32) as usize & (self.buckets.len() - 1)
    }

    fn find(&self, tag: u64) -> Option<usize> {
        let mut current = self.buckets[self.bucket(tag)];
        while current != NO_LINE {
            let line = &self.lines[current as usize];
            if line.tag == tag {
                return Some(current as usize);
            }
            current = line.chain;
        }
        None
    }

    fn unlink(&mut self, index: usize) {
        let tag = self.lines[index].tag;
        if tag == 0 {
            return;
        }
        let bucket = self.bucket(tag);
        let mut current = self.buckets[bucket];
        let mut previous = NO_LINE;
        while current != NO_LINE {
            let next = self.lines[current as usize].chain;
            if current as usize == index {
                if previous == NO_LINE {
                    self.buckets[bucket] = next;
                } else {
                    self.lines[previous as usize].chain = next;
                }
                break;
            }
            previous = current;
            current = next;
        }
        self.lines[index].tag = 0;
        self.lines[index].chain = NO_LINE;
    }

    fn link(&mut self, index: usize, tag: u64) {
        let bucket = self.bucket(tag);
        self.lines[index].tag = tag;
        self.lines[index].chain = self.buckets[bucket];
        self.buckets[bucket] = index as u32;
    }

    fn slice(&self, index: usize) -> &[u8] {
        let start = index * self.line_bytes;
        &self.data[start..start + self.line_bytes]
    }

    /// Blocks actually backing line `tag`; the disk's last line can be short.
    fn line_blocks(&self, tag: u64) -> u64 {
        let first = (tag - 1) * self.blocks_per_line;
        self.blocks_per_line.min(self.block_count.saturating_sub(first))
    }

    fn read_line(&mut self, index: usize, tag: u64) -> Result<(), FsError> {
        let first = (tag - 1) * self.blocks_per_line;
        let bytes = self.line_blocks(tag) as usize * self.block_size;
        let start = index * self.line_bytes;
        self.stats.read_misses += 1;
        self.device
            .read_blocks(first, &mut self.data[start..start + bytes])
    }
}

impl<D: WritableBlockDevice> Inner<D> {
    /// Writes a line's dirty blocks of `class`, each run as one request.
    fn write_line(&mut self, index: usize, class: WriteClass) -> Result<(), FsError> {
        let first = (self.lines[index].tag - 1) * self.blocks_per_line;
        let start = index * self.line_bytes;
        let mut block = 0usize;
        while block < self.blocks_per_line as usize {
            if self.lines[index].dirty[block] != Some(class) {
                block += 1;
                continue;
            }
            let mut end = block + 1;
            while end < self.blocks_per_line as usize && self.lines[index].dirty[end] == Some(class)
            {
                end += 1;
            }
            let bytes = start + block * self.block_size..start + end * self.block_size;
            self.device
                .write_blocks(first + block as u64, &self.data[bytes])?;
            self.stats.device_writes += 1;
            for slot in &mut self.lines[index].dirty[block..end] {
                *slot = None;
            }
            self.dirty[class_index(class)] -= end - block;
            block = end;
        }
        Ok(())
    }

    fn device_flush(&mut self) -> Result<(), FsError> {
        self.stats.flushes += 1;
        match self.device.flush() {
            Ok(()) => {
                self.unflushed = false;
                Ok(())
            }
            Err(error) => {
                self.unflushed = true;
                Err(error)
            }
        }
    }

    fn commit(&mut self) -> Result<(), FsError> {
        let mut retry_flush = self.unflushed;
        for class in CLASSES {
            if self.dirty[class_index(class)] == 0 {
                continue;
            }
            // In disk order, which is kinder to anything that has a seek.
            let mut order: Vec<(u64, u32)> = Vec::new();
            order
                .try_reserve_exact(self.lines.len().min(self.dirty[class_index(class)]))
                .map_err(|_| FsError::OutOfMemory)?;
            for (index, line) in self.lines.iter().enumerate() {
                if line.dirty.contains(&Some(class)) {
                    order.push((line.tag, index as u32));
                }
            }
            order.sort_unstable();
            for (_, index) in order {
                self.write_line(index as usize, class)?;
            }
            // The barrier: nothing of the next class starts until this is safe.
            self.device_flush()?;
            retry_flush = false;
        }
        if retry_flush {
            self.device_flush()?;
        }
        Ok(())
    }

    /// Frees a line for reuse, writing it out or committing as order requires.
    fn victim(&mut self) -> Result<usize, FsError> {
        let total = self.lines.len();
        // Two passes of the hand give used lines a second chance.
        for _ in 0..total * 2 {
            let index = self.hand;
            self.hand = (self.hand + 1) % total;
            let line = &mut self.lines[index];
            if line.tag != 0 && line.referenced {
                line.referenced = false;
                continue;
            }
            if line.dirty == [None; MAX_BLOCKS_PER_LINE] {
                self.unlink(index);
                return Ok(index);
            }
        }
        // Everything clean is gone. File data can go out of order.
        for offset in 0..total {
            let index = (self.hand + offset) % total;
            let line = &self.lines[index];
            let only_data = line
                .dirty
                .iter()
                .all(|slot| matches!(slot, None | Some(WriteClass::Data)));
            if only_data && line.tag != 0 {
                self.write_line(index, WriteClass::Data)?;
                self.unlink(index);
                self.hand = (index + 1) % total;
                return Ok(index);
            }
        }
        // Only metadata is left, and it has to go in order.
        self.stats.forced_commits += 1;
        self.commit()?;
        let index = self.hand;
        self.hand = (self.hand + 1) % total;
        self.unlink(index);
        Ok(index)
    }

    /// The line for `tag`, loaded unless the caller will overwrite all of it.
    fn line_for_write(&mut self, tag: u64, whole: bool) -> Result<usize, FsError> {
        if let Some(index) = self.find(tag) {
            return Ok(index);
        }
        let index = self.victim()?;
        if !whole {
            self.read_line(index, tag)?;
        }
        self.link(index, tag);
        Ok(index)
    }

    /// Marks blocks of a line dirty. A block rewritten as a later class waits for
    /// that class; one rewritten as an earlier class must not jump ahead.
    fn mark_dirty(&mut self, index: usize, blocks: core::ops::Range<usize>, class: WriteClass) {
        for block in blocks {
            let current = self.lines[index].dirty[block];
            let merged = match current {
                Some(existing) if existing >= class => existing,
                _ => class,
            };
            if let Some(existing) = current {
                self.dirty[class_index(existing)] -= 1;
            }
            self.dirty[class_index(merged)] += 1;
            self.lines[index].dirty[block] = Some(merged);
        }
        self.lines[index].referenced = true;
    }

    fn forget_line(&mut self, index: usize) {
        for slot in 0..MAX_BLOCKS_PER_LINE {
            if let Some(dirty) = self.lines[index].dirty[slot].take() {
                self.dirty[class_index(dirty)] -= 1;
            }
        }
        self.unlink(index);
    }

    fn write(&mut self, start: u64, data: &[u8], class: WriteClass) -> Result<(), FsError> {
        let blocks = (data.len() / self.block_size) as u64;
        let end = start.checked_add(blocks).ok_or(FsError::OutOfBounds)?;
        if !data.len().is_multiple_of(self.block_size) {
            return Err(FsError::InvalidBuffer);
        }
        if end > self.block_count {
            return Err(FsError::OutOfBounds);
        }

        let per_line = self.blocks_per_line;
        let mut block = start;
        let mut done = 0usize;
        while block < end {
            let tag = block / per_line + 1;
            let within = (block % per_line) as usize * self.block_size;
            let line_blocks = self.line_blocks(tag);
            let in_line = (line_blocks - block % per_line).min(end - block);

            // File data this large would only flush the cache through itself.
            if class == WriteClass::Data && within == 0 {
                let whole = (end - block) / per_line;
                if whole >= BYPASS_LINES {
                    let count = whole * per_line;
                    for offset in 0..whole {
                        if let Some(index) = self.find(tag + offset) {
                            self.forget_line(index);
                        }
                    }
                    let bytes = count as usize * self.block_size;
                    self.device
                        .write_blocks(block, &data[done..done + bytes])?;
                    self.stats.device_writes += whole;
                    block += count;
                    done += bytes;
                    continue;
                }
            }

            let bytes = in_line as usize * self.block_size;
            let whole = in_line == line_blocks;
            let index = self.line_for_write(tag, whole)?;
            let line_start = index * self.line_bytes + within;
            self.data[line_start..line_start + bytes].copy_from_slice(&data[done..done + bytes]);
            let first = (block % per_line) as usize;
            self.mark_dirty(index, first..first + in_line as usize, class);
            block += in_line;
            done += bytes;
        }
        Ok(())
    }

    fn read(&mut self, start: u64, out: &mut [u8]) -> Result<(), FsError> {
        let blocks = (out.len() / self.block_size) as u64;
        let end = start.checked_add(blocks).ok_or(FsError::OutOfBounds)?;
        if !out.len().is_multiple_of(self.block_size) {
            return Err(FsError::InvalidBuffer);
        }
        if end > self.block_count {
            return Err(FsError::OutOfBounds);
        }

        let per_line = self.blocks_per_line;
        let mut block = start;
        let mut done = 0usize;
        while block < end {
            let tag = block / per_line + 1;
            let within = (block % per_line) as usize * self.block_size;
            let in_line = (self.line_blocks(tag) - block % per_line).min(end - block);
            let bytes = in_line as usize * self.block_size;

            let index = match self.find(tag) {
                Some(index) => {
                    self.stats.read_hits += 1;
                    index
                }
                None => {
                    // A run of uncached lines goes straight into the caller's buffer.
                    if within == 0 {
                        let mut run = 0u64;
                        let whole = (end - block) / per_line;
                        while run < whole && self.find(tag + run).is_none() {
                            run += 1;
                        }
                        if run >= BYPASS_LINES {
                            let count = run * per_line;
                            let total = count as usize * self.block_size;
                            self.stats.read_misses += run;
                            self.device
                                .read_blocks(block, &mut out[done..done + total])?;
                            block += count;
                            done += total;
                            continue;
                        }
                    }
                    let index = self.victim()?;
                    self.read_line(index, tag)?;
                    self.link(index, tag);
                    index
                }
            };
            let line = self.slice(index);
            out[done..done + bytes].copy_from_slice(&line[within..within + bytes]);
            self.lines[index].referenced = true;
            block += in_line;
            done += bytes;
        }
        Ok(())
    }
}

impl<D: WritableBlockDevice + Send> BlockDevice for BufferCache<D> {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        self.read_blocks(block, out)
    }

    fn read_blocks(&self, start: u64, out: &mut [u8]) -> Result<(), FsError> {
        self.inner.lock().read(start, out)
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

impl<D: WritableBlockDevice + Send> WritableBlockDevice for BufferCache<D> {
    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError> {
        self.write_classified(block, data, WriteClass::Data)
    }

    fn write_blocks(&mut self, start: u64, data: &[u8]) -> Result<(), FsError> {
        self.write_classified(start, data, WriteClass::Data)
    }

    fn write_classified(
        &mut self,
        start: u64,
        data: &[u8],
        class: WriteClass,
    ) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        self.inner.lock().write(start, data, class)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        self.commit()
    }

    fn commit(&mut self) -> Result<(), FsError> {
        if self.read_only {
            return Ok(());
        }
        self.inner.lock().commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::vec;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Event {
        Write(u64, usize),
        Flush,
    }

    /// A disk that logs what reaches it, and can start failing.
    struct Disk {
        data: Vec<u8>,
        log: Arc<StdMutex<Vec<Event>>>,
        reads: Arc<StdMutex<Vec<(u64, usize)>>>,
        fail_flush: Arc<StdMutex<bool>>,
    }

    impl Disk {
        fn new(blocks: usize) -> Self {
            Self {
                data: vec![0; blocks * 512],
                log: Arc::default(),
                reads: Arc::default(),
                fail_flush: Arc::default(),
            }
        }
    }

    impl BlockDevice for Disk {
        fn block_size(&self) -> usize {
            512
        }
        fn block_count(&self) -> u64 {
            (self.data.len() / 512) as u64
        }
        fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
            self.read_blocks(block, out)
        }
        fn read_blocks(&self, start: u64, out: &mut [u8]) -> Result<(), FsError> {
            self.reads.lock().unwrap().push((start, out.len()));
            let at = start as usize * 512;
            out.copy_from_slice(&self.data[at..at + out.len()]);
            Ok(())
        }
    }

    impl WritableBlockDevice for Disk {
        fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError> {
            self.write_blocks(block, data)
        }
        fn write_blocks(&mut self, start: u64, data: &[u8]) -> Result<(), FsError> {
            self.log.lock().unwrap().push(Event::Write(start, data.len()));
            let at = start as usize * 512;
            self.data[at..at + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), FsError> {
            self.log.lock().unwrap().push(Event::Flush);
            if *self.fail_flush.lock().unwrap() {
                return Err(FsError::Io);
            }
            Ok(())
        }
    }

    fn cache(blocks: usize, lines: usize) -> (BufferCache<Disk>, Arc<StdMutex<Vec<Event>>>) {
        let disk = Disk::new(blocks);
        let log = disk.log.clone();
        (BufferCache::new(disk, lines * 4096).unwrap(), log)
    }

    #[test]
    fn a_write_is_visible_at_once_and_reaches_the_disk_on_commit() {
        let (mut cache, log) = cache(64, 16);
        cache.write_block(3, &[7; 512]).unwrap();
        assert!(log.lock().unwrap().is_empty(), "write-back holds it");
        let mut block = [0; 512];
        cache.read_block(3, &mut block).unwrap();
        assert_eq!(block, [7; 512]);
        cache.commit().unwrap();
        assert_eq!(cache.dirty_blocks(), 0);
        assert_eq!(cache.into_inner().data[3 * 512], 7);
    }

    #[test]
    fn repeated_writes_to_a_line_merge_into_one() {
        let (mut cache, log) = cache(64, 16);
        for round in 0..100u8 {
            cache.write_classified(1, &[round; 512], WriteClass::Dir).unwrap();
        }
        cache.commit().unwrap();
        let writes = log
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, Event::Write(..)))
            .count();
        assert_eq!(writes, 1);
    }

    #[test]
    fn commit_writes_data_then_fat_then_directories_with_barriers() {
        let (mut cache, log) = cache(128, 16);
        cache.write_classified(32, &[1; 512], WriteClass::Dir).unwrap();
        cache.write_classified(16, &[1; 512], WriteClass::Fat).unwrap();
        cache.write_classified(8, &[1; 512], WriteClass::Data).unwrap();
        cache.commit().unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [
                Event::Write(8, 512),
                Event::Flush,
                Event::Write(16, 512),
                Event::Flush,
                Event::Write(32, 512),
                Event::Flush,
            ]
        );
    }

    #[test]
    fn blocks_of_one_line_leave_in_class_order() {
        // A new directory's cluster beside its parent's entries: the cluster has
        // to land first, or the entry points at stale bytes.
        let (mut cache, log) = cache(128, 16);
        cache.write_classified(8, &[1; 512], WriteClass::Dir).unwrap();
        cache.write_classified(9, &[1; 512], WriteClass::Data).unwrap();
        cache.write_classified(10, &[1; 512], WriteClass::Data).unwrap();
        cache.commit().unwrap();
        assert_eq!(
            log.lock().unwrap()[..3],
            [Event::Write(9, 1024), Event::Flush, Event::Write(8, 512)]
        );
    }

    #[test]
    fn a_block_rewritten_as_an_earlier_class_keeps_its_later_one() {
        let (mut cache, log) = cache(128, 16);
        cache.write_classified(8, &[1; 512], WriteClass::Dir).unwrap();
        cache.write_classified(8, &[2; 512], WriteClass::Data).unwrap();
        cache.write_classified(16, &[1; 512], WriteClass::Fat).unwrap();
        cache.commit().unwrap();
        assert_eq!(
            log.lock().unwrap()[..3],
            [Event::Write(16, 512), Event::Flush, Event::Write(8, 512)]
        );
    }

    #[test]
    fn a_full_cache_evicts_data_without_a_commit_but_keeps_metadata_ordered() {
        let (mut cache, log) = cache(2048, 16);
        for line in 0..32u64 {
            cache.write_classified(line * 8, &[1; 512], WriteClass::Data).unwrap();
        }
        assert_eq!(cache.stats().forced_commits, 0);
        assert!(!log.lock().unwrap().contains(&Event::Flush));
        for line in 100..132u64 {
            cache.write_classified(line * 8, &[1; 512], WriteClass::Dir).unwrap();
        }
        assert!(cache.stats().forced_commits > 0);
        // Everything comes back through eviction.
        let mut block = [0; 512];
        for line in 100..132u64 {
            cache.read_block(line * 8, &mut block).unwrap();
            assert_eq!(block, [1; 512]);
        }
    }

    #[test]
    fn a_large_read_goes_to_the_device_as_one_request() {
        let disk = Disk::new(1024);
        let reads = disk.reads.clone();
        let cache = BufferCache::new(disk, 16 * 4096).unwrap();
        let mut out = vec![0; 64 * 4096 / 8];
        cache.read_blocks(0, &mut out).unwrap();
        assert_eq!(reads.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_large_write_replaces_cached_lines() {
        let (mut cache, _log) = cache(1024, 16);
        cache.write_classified(8, &[1; 512], WriteClass::Dir).unwrap();
        assert_eq!(cache.dirty_blocks(), 1);
        let data = vec![9u8; 16 * 4096];
        cache.write_blocks(0, &data).unwrap();
        assert_eq!(cache.dirty_blocks(), 0);
        let mut block = [0; 512];
        cache.read_block(8, &mut block).unwrap();
        assert_eq!(block, [9; 512]);
    }

    #[test]
    fn the_short_last_line_of_a_disk_works() {
        let (mut cache, _log) = cache(10, 16);
        cache.write_block(9, &[3; 512]).unwrap();
        cache.commit().unwrap();
        let mut block = [0; 512];
        cache.read_block(9, &mut block).unwrap();
        assert_eq!(block, [3; 512]);
        assert_eq!(cache.read_block(10, &mut block), Err(FsError::OutOfBounds));
    }

    #[test]
    fn a_failed_flush_is_reported_again_until_one_succeeds() {
        let disk = Disk::new(64);
        let fail = disk.fail_flush.clone();
        let mut cache = BufferCache::new(disk, 16 * 4096).unwrap();
        cache.write_block(0, &[1; 512]).unwrap();
        *fail.lock().unwrap() = true;
        assert_eq!(cache.commit(), Err(FsError::Io));
        assert_eq!(cache.commit(), Err(FsError::Io));
        *fail.lock().unwrap() = false;
        assert_eq!(cache.commit(), Ok(()));
    }
}
