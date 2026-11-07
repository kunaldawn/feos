//! FEOS's own filesystem: inodes with POSIX metadata, and a journal.
//!
//! FAT and exFAT can't say who owns a file or hold a hard link, and only stay consistent
//! through a check after a crash. This can do both, and needs no check.
//!
//! # On disk
//!
//! All little-endian, in blocks of 4 KiB, whatever the device's are:
//!
//! ```text
//! block 0            superblock, with a checksum
//! bitmap             one bit per block of the volume; set means in use
//! inode table        128 bytes each, numbered from 1. Mode zero means free
//! journal            one transaction at a time, see below
//! data               file contents, directories and indirect blocks
//! ```
//!
//! An inode has mode, owner, group, link count, size and times, and 12 direct, one indirect
//! and one double indirect block pointers, with zero meaning a hole. A directory is a
//! file of fixed 256-byte entries: inode number, kind, and a name of up to 250 bytes. A
//! symbolic link is what the VFS makes it, a file holding its target, with `S_IFLNK` set.
//!
//! # Crashes
//!
//! Everything but file contents goes through the journal, as whole blocks: the bitmap, the
//! inode table, directories and indirect blocks. Changes collect in memory as an open
//! transaction, only ever between whole operations, and committing it is:
//!
//! 1. Flush, so that the file data the metadata will point at is on the disk.
//! 2. Write the blocks to the journal after a descriptor saying where they go. Flush.
//! 3. Write a commit block with their checksum. Flush. It has now happened.
//! 4. Write the blocks to where they belong. Flush.
//! 5. Clear the descriptor. Flush.
//!
//! Mounting replays a transaction whose commit block checks out, and ignores anything else, so
//! a crash leaves all of a transaction or none. Replaying twice does no harm.
//!
//! Three things make that work. File data is ordered ahead of what refers to it, so a file can't
//! show another's old contents. A block freed in the open transaction isn't reused until it has
//! committed, so if that transaction is lost the old file's data is still there. And a file's
//! tail is zeroed by shrinking through the journal, with the size.
//!
//! What it doesn't promise: overwriting existing file data in place isn't journalled, so a crash
//! during one can leave some of the new data and some of the old, a sector at a time. Anything
//! not yet synced can be lost, though never half an operation. `VfsTree::write_file_atomic`
//! is how to replace a file all at once.
//!
//! `libs/fs/tests/native.rs` crashes at every write that reaches the device, with writes since
//! the last flush landing or not at random.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::block::WritableBlockDevice;
use crate::partition::crc32;
use crate::tree::try_zeroed;
use crate::{attr, CheckReport, EntryKind, FileSystem, FsError, NodeId, NodeTimes, TimeSource, VolumeStats, WritableFileSystem};

/// What the first eight bytes of the volume say.
pub const MAGIC: &[u8; 8] = b"FEOSNFS1";

const VERSION: u32 = 1;
/// The filesystem's block size.
pub const BLOCK: usize = 4096;
const INODE_SIZE: usize = 128;
const INODES_PER_BLOCK: u32 = (BLOCK / INODE_SIZE) as u32;
const ENTRY_SIZE: usize = 256;
const ENTRIES_PER_BLOCK: usize = BLOCK / ENTRY_SIZE;
/// Longest name, in bytes.
pub const NAME_MAX: usize = 250;
const DIRECT: usize = 12;
const POINTERS: usize = BLOCK / 4;
const ROOT_INODE: u32 = 1;
const MAX_NODES: usize = 1 << 20;
const MAX_DEPTH: usize = 64;
/// The smallest volume that has room for its own metadata and something else.
const MIN_BLOCKS: u32 = 64;

const JOURNAL_DESCRIPTOR: &[u8; 8] = b"FJRNDESC";
const JOURNAL_COMMIT: &[u8; 8] = b"FJRNCMIT";
/// Homes a descriptor holds: what's left of a block after its header.
const JOURNAL_MAX_BLOCKS: u32 = ((BLOCK - 24) / 4) as u32;

/// The type bits of a mode.
pub const S_IFMT: u16 = 0o170000;
/// A regular file.
pub const S_IFREG: u16 = 0o100000;
/// A directory.
pub const S_IFDIR: u16 = 0o040000;
/// A symbolic link.
pub const S_IFLNK: u16 = 0o120000;
/// Permission bits a caller can set, with setuid, setgid and sticky.
pub const MODE_BITS: u16 = 0o7777;

const SYMLINK_MAGIC: &[u8] = b"XSym\n";

/// What a file has that FAT can't say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PosixMeta {
    /// Type and permission bits.
    pub mode: u16,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Directory entries for this inode. For a directory, two plus its subdirectories.
    pub nlink: u32,
    /// Inode number.
    pub ino: u64,
}

/// What `fsck` found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsckReport {
    /// Each thing wrong, in words.
    pub errors: Vec<String>,
    /// How many were put right.
    pub repaired: usize,
    /// Regular files and links.
    pub files: u32,
    /// Directories.
    pub directories: u32,
    /// Blocks in use, metadata included.
    pub used_blocks: u32,
    /// Blocks in the volume.
    pub total_blocks: u32,
}

impl FsckReport {
    /// True when nothing is wrong that wasn't repaired.
    pub fn clean(&self) -> bool {
        self.errors.len() == self.repaired
    }
}

fn get_u16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn get_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn get_u64(data: &[u8], at: usize) -> u64 {
    u64::from(get_u32(data, at)) | u64::from(get_u32(data, at + 4)) << 32
}

fn put_u16(data: &mut [u8], at: usize, value: u16) {
    data[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(data: &mut [u8], at: usize, value: u64) {
    data[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn new_block() -> Result<Box<[u8; BLOCK]>, FsError> {
    let vector = try_zeroed(BLOCK)?;
    vector.into_boxed_slice().try_into().map_err(|_| FsError::OutOfMemory)
}

/// Where everything is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Layout {
    blocks: u32,
    inodes: u32,
    bitmap_start: u32,
    bitmap_blocks: u32,
    inode_start: u32,
    inode_blocks: u32,
    journal_start: u32,
    journal_blocks: u32,
    data_start: u32,
}

impl Layout {
    /// What a volume of this size gets. `scripts/mkfs-feos.py` has the same sums.
    fn for_blocks(blocks: u32) -> Self {
        let bitmap_blocks = blocks.div_ceil(BLOCK as u32 * 8);
        let inode_blocks = (blocks / 64).max(4);
        let journal_blocks = (blocks / 32).clamp(32, JOURNAL_MAX_BLOCKS + 2);
        let bitmap_start = 1;
        let inode_start = bitmap_start + bitmap_blocks;
        let journal_start = inode_start + inode_blocks;
        Self {
            blocks,
            inodes: inode_blocks * INODES_PER_BLOCK,
            bitmap_start,
            bitmap_blocks,
            inode_start,
            inode_blocks,
            journal_start,
            journal_blocks,
            data_start: journal_start + journal_blocks,
        }
    }

    fn encode(&self, created: u64) -> [u8; BLOCK] {
        let mut block = [0u8; BLOCK];
        block[..8].copy_from_slice(MAGIC);
        put_u32(&mut block, 8, VERSION);
        put_u32(&mut block, 12, BLOCK as u32);
        for (index, value) in [
            self.blocks,
            self.inodes,
            self.bitmap_start,
            self.bitmap_blocks,
            self.inode_start,
            self.inode_blocks,
            self.journal_start,
            self.journal_blocks,
            self.data_start,
        ]
        .into_iter()
        .enumerate()
        {
            put_u32(&mut block, 16 + index * 4, value);
        }
        put_u64(&mut block, 52, created);
        let crc = crc32(&block[..508]);
        put_u32(&mut block, 508, crc);
        block
    }

    /// Reads a superblock, none of which is trusted until it adds up.
    fn decode(block: &[u8]) -> Result<Self, FsError> {
        if &block[..8] != MAGIC || get_u32(block, 8) != VERSION || get_u32(block, 12) != BLOCK as u32 {
            return Err(FsError::InvalidBootSector);
        }
        if crc32(&block[..508]) != get_u32(block, 508) {
            return Err(FsError::Corrupt);
        }
        let field = |index: usize| get_u32(block, 16 + index * 4);
        let layout = Self {
            blocks: field(0),
            inodes: field(1),
            bitmap_start: field(2),
            bitmap_blocks: field(3),
            inode_start: field(4),
            inode_blocks: field(5),
            journal_start: field(6),
            journal_blocks: field(7),
            data_start: field(8),
        };
        if layout != Self::for_blocks(layout.blocks) || layout.blocks < MIN_BLOCKS || layout.data_start >= layout.blocks {
            return Err(FsError::Corrupt);
        }
        Ok(layout)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Inode {
    mode: u16,
    attributes: u8,
    uid: u32,
    gid: u32,
    nlink: u32,
    size: u64,
    modified: u64,
    changed: u64,
    created: u64,
    direct: [u32; DIRECT],
    indirect: u32,
    double: u32,
}

impl Inode {
    fn encode(&self, out: &mut [u8]) {
        out[..INODE_SIZE].fill(0);
        put_u16(out, 0, self.mode);
        out[2] = self.attributes;
        put_u32(out, 4, self.uid);
        put_u32(out, 8, self.gid);
        put_u32(out, 12, self.nlink);
        put_u64(out, 16, self.size);
        put_u64(out, 24, self.modified);
        put_u64(out, 32, self.changed);
        put_u64(out, 40, self.created);
        for (index, pointer) in self.direct.iter().enumerate() {
            put_u32(out, 48 + index * 4, *pointer);
        }
        put_u32(out, 96, self.indirect);
        put_u32(out, 100, self.double);
        let crc = crc32(&out[..124]);
        put_u32(out, 124, crc);
    }

    /// `None` when the checksum says it's not what was written.
    fn decode(data: &[u8]) -> Option<Self> {
        if crc32(&data[..124]) != get_u32(data, 124) {
            return None;
        }
        let mut direct = [0u32; DIRECT];
        for (index, pointer) in direct.iter_mut().enumerate() {
            *pointer = get_u32(data, 48 + index * 4);
        }
        Some(Self {
            mode: get_u16(data, 0),
            attributes: data[2],
            uid: get_u32(data, 4),
            gid: get_u32(data, 8),
            nlink: get_u32(data, 12),
            size: get_u64(data, 16),
            modified: get_u64(data, 24),
            changed: get_u64(data, 32),
            created: get_u64(data, 40),
            direct,
            indirect: get_u32(data, 96),
            double: get_u32(data, 100),
        })
    }

    fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    fn kind(&self) -> EntryKind {
        if self.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        }
    }
}

/// A directory entry as a node: so a file with two names is two of these, on one inode.
struct Node {
    name: String,
    parent: u32,
    ino: u32,
    children: Vec<u32>,
    /// Byte position of the entry in its parent's data.
    slot: u64,
}

/// Formats a block device. Whatever was on it has gone.
pub fn format<D: WritableBlockDevice>(device: &mut D, created: u64) -> Result<(), FsError> {
    let ratio = device_ratio(device)?;
    let blocks = u32::try_from(device.block_count() / ratio).unwrap_or(u32::MAX);
    if blocks < MIN_BLOCKS {
        return Err(FsError::NoSpace);
    }
    let layout = Layout::for_blocks(blocks);
    let zero = new_block()?;
    let write = |device: &mut D, block: u32, data: &[u8]| device.write_blocks(u64::from(block) * ratio, data);

    // Clearing the journal comes first, or an old volume's could replay onto this one.
    write(device, layout.journal_start, &zero[..])?;
    for block in layout.inode_start..layout.inode_start + layout.inode_blocks {
        write(device, block, &zero[..])?;
    }
    // Every block of metadata is in use, and the root's is the only inode.
    for index in 0..layout.bitmap_blocks {
        let mut bitmap = new_block()?;
        let first = index * BLOCK as u32 * 8;
        for bit in 0..BLOCK as u32 * 8 {
            let block = first + bit;
            // Past the end counts as used, so nothing can allocate it.
            if block < layout.data_start || block >= blocks {
                bitmap[bit as usize / 8] |= 1 << (bit % 8);
            }
        }
        write(device, layout.bitmap_start + index, &bitmap[..])?;
    }
    let root = Inode {
        mode: S_IFDIR | 0o755,
        nlink: 1,
        modified: created,
        changed: created,
        created,
        ..Inode::default()
    };
    let mut table = new_block()?;
    root.encode(&mut table[..INODE_SIZE]);
    write(device, layout.inode_start, &table[..])?;
    device.flush()?;
    // The superblock last: until it's there, this isn't a volume.
    write(device, 0, &layout.encode(created))?;
    device.flush()
}

/// Device blocks per filesystem block.
fn device_ratio<D: crate::block::BlockDevice>(device: &D) -> Result<u64, FsError> {
    let size = device.block_size();
    if size == 0 || size > BLOCK || BLOCK % size != 0 {
        return Err(FsError::UnsupportedBlockSize);
    }
    Ok((BLOCK / size) as u64)
}

/// A mounted volume.
pub struct Native<D: WritableBlockDevice> {
    device: D,
    ratio: u64,
    layout: Layout,
    /// Blocks that can't be allocated: what the bitmap says, and what's been freed in the
    /// open transaction, which the bitmap on disk still shows as in use.
    allocated: Vec<u64>,
    next_free: u32,
    /// Freed in the open transaction.
    pending_free: Vec<u32>,
    inodes: BTreeMap<u32, Inode>,
    next_inode: u32,
    /// Metadata changed in the open transaction: home block to contents.
    open: BTreeMap<u32, Box<[u8; BLOCK]>>,
    sequence: u64,
    nodes: Vec<Option<Node>>,
    free_nodes: Vec<u32>,
    live_nodes: usize,
    now: Option<TimeSource>,
    damaged: bool,
    /// A commit failed, and what's in memory may not be what's on the disk.
    failed: bool,
    replayed: bool,
}

impl<D: WritableBlockDevice> Native<D> {
    /// Mounts a volume: replays what the journal holds, and reads the metadata in.
    pub fn mount(device: D) -> Result<Self, FsError> {
        let ratio = device_ratio(&device)?;
        let mut first = new_block()?;
        device.read_blocks(0, &mut first[..])?;
        let layout = Layout::decode(&first[..])?;
        if u64::from(layout.blocks) * ratio > device.block_count() {
            return Err(FsError::Corrupt);
        }
        let words = (layout.blocks as usize).div_ceil(64);
        let mut allocated = Vec::new();
        allocated.try_reserve_exact(words).map_err(|_| FsError::OutOfMemory)?;
        allocated.resize(words, 0u64);
        let mut fs = Self {
            device,
            ratio,
            layout,
            allocated,
            next_free: layout.data_start,
            pending_free: Vec::new(),
            inodes: BTreeMap::new(),
            next_inode: 1,
            open: BTreeMap::new(),
            sequence: 1,
            nodes: Vec::new(),
            free_nodes: Vec::new(),
            live_nodes: 0,
            now: None,
            damaged: false,
            failed: false,
            replayed: false,
        };
        fs.replay()?;
        fs.load_bitmap()?;
        fs.load_inodes()?;
        fs.load_tree()?;
        Ok(fs)
    }

    /// Gives the device back. Whatever hasn't been flushed is lost.
    pub fn into_device(self) -> D {
        self.device
    }

    fn read_home(&self, block: u32, out: &mut [u8]) -> Result<(), FsError> {
        if block >= self.layout.blocks {
            return Err(FsError::Corrupt);
        }
        self.device.read_blocks(u64::from(block) * self.ratio, out)
    }

    fn write_home(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> {
        if block >= self.layout.blocks {
            return Err(FsError::Corrupt);
        }
        self.device.write_blocks(u64::from(block) * self.ratio, data)
    }

    /// Reads a block as this transaction sees it.
    fn read_meta(&self, block: u32, out: &mut [u8]) -> Result<(), FsError> {
        match self.open.get(&block) {
            Some(data) => {
                out.copy_from_slice(&data[..]);
                Ok(())
            }
            None => self.read_home(block, out),
        }
    }

    /// A block's contents to change, in the open transaction.
    fn meta_mut(&mut self, block: u32) -> Result<&mut [u8; BLOCK], FsError> {
        if !self.open.contains_key(&block) {
            let mut data = new_block()?;
            self.read_home(block, &mut data[..])?;
            self.open.insert(block, data);
        }
        self.open.get_mut(&block).map(|data| &mut **data).ok_or(FsError::Io)
    }

    /// Applies a transaction that made it to the journal, if one did.
    fn replay(&mut self) -> Result<(), FsError> {
        let start = self.layout.journal_start;
        let mut descriptor = new_block()?;
        self.read_home(start, &mut descriptor[..])?;
        if &descriptor[..8] != JOURNAL_DESCRIPTOR {
            return Ok(());
        }
        let sequence = get_u64(&descriptor[..], 8);
        let count = get_u32(&descriptor[..], 16);
        if count > JOURNAL_MAX_BLOCKS || count + 2 > self.layout.journal_blocks {
            return Ok(());
        }
        let mut commit = new_block()?;
        self.read_home(start + 1 + count, &mut commit[..])?;
        if &commit[..8] != JOURNAL_COMMIT
            || get_u64(&commit[..], 8) != sequence
            || get_u32(&commit[..], 16) != count
        {
            return Ok(());
        }
        // The descriptor and everything it brings have to be what the commit block says.
        let mut crc_input = Vec::new();
        crc_input
            .try_reserve_exact((count as usize + 1) * BLOCK)
            .map_err(|_| FsError::OutOfMemory)?;
        crc_input.extend_from_slice(&descriptor[..]);
        let mut block = new_block()?;
        for index in 0..count {
            self.read_home(start + 1 + index, &mut block[..])?;
            crc_input.extend_from_slice(&block[..]);
        }
        if crc32(&crc_input) != get_u32(&commit[..], 20) {
            return Ok(());
        }
        // Checksummed, so this came from here. Still, nothing outside the metadata and
        // data areas is a thing a transaction writes.
        for index in 0..count as usize {
            let home = get_u32(&descriptor[..], 24 + index * 4);
            if home == 0 || home >= self.layout.blocks || (home >= start && home < self.layout.data_start) {
                return Err(FsError::Corrupt);
            }
        }
        for index in 0..count as usize {
            let home = get_u32(&descriptor[..], 24 + index * 4);
            let from = (index + 1) * BLOCK;
            let image = &crc_input[from..from + BLOCK];
            if self.device.is_read_only() {
                let mut copy = new_block()?;
                copy.copy_from_slice(image);
                self.open.insert(home, copy);
            } else {
                self.write_home(home, image)?;
            }
        }
        self.sequence = sequence + 1;
        self.replayed = true;
        if !self.device.is_read_only() {
            self.device.flush()?;
            let zero = new_block()?;
            self.write_home(start, &zero[..])?;
            self.device.flush()?;
        }
        Ok(())
    }

    fn load_bitmap(&mut self) -> Result<(), FsError> {
        let mut block = new_block()?;
        for index in 0..self.layout.bitmap_blocks {
            self.read_meta(self.layout.bitmap_start + index, &mut block[..])?;
            for (word_index, chunk) in block.chunks_exact(8).enumerate() {
                let at = index as usize * (BLOCK / 8) + word_index;
                if let Some(word) = self.allocated.get_mut(at) {
                    *word = get_u64(chunk, 0);
                }
            }
        }
        // Metadata isn't available whatever the bitmap says.
        for block in 0..self.layout.data_start {
            self.set_allocated(block, true);
        }
        Ok(())
    }

    fn load_inodes(&mut self) -> Result<(), FsError> {
        let mut block = new_block()?;
        for index in 0..self.layout.inode_blocks {
            self.read_meta(self.layout.inode_start + index, &mut block[..])?;
            for slot in 0..INODES_PER_BLOCK {
                let data = &block[slot as usize * INODE_SIZE..(slot as usize + 1) * INODE_SIZE];
                if data.iter().all(|byte| *byte == 0) {
                    continue;
                }
                let ino = index * INODES_PER_BLOCK + slot + 1;
                match Inode::decode(data) {
                    Some(inode) if inode.mode != 0 => {
                        self.inodes.insert(ino, inode);
                    }
                    Some(_) => {}
                    None => self.damaged = true,
                }
            }
        }
        Ok(())
    }

    fn load_tree(&mut self) -> Result<(), FsError> {
        let root = *self.inodes.get(&ROOT_INODE).ok_or(FsError::Corrupt)?;
        if !root.is_dir() {
            return Err(FsError::Corrupt);
        }
        self.nodes.push(Some(Node {
            name: String::from("/"),
            parent: 0,
            ino: ROOT_INODE,
            children: Vec::new(),
            slot: 0,
        }));
        self.live_nodes = 1;
        let mut queue: Vec<(u32, usize)> = alloc::vec![(0, 0)];
        let mut seen_directories = BTreeMap::new();
        seen_directories.insert(ROOT_INODE, ());
        let mut block = new_block()?;
        while let Some((node, depth)) = queue.pop() {
            let ino = self.nodes[node as usize].as_ref().ok_or(FsError::Corrupt)?.ino;
            let inode = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
            if inode.size % BLOCK as u64 != 0 {
                self.damaged = true;
                continue;
            }
            for index in 0..inode.size / BLOCK as u64 {
                let Ok(home) = self.block_of(&inode, index) else {
                    self.damaged = true;
                    continue;
                };
                if home == 0 {
                    continue;
                }
                self.read_meta(home, &mut block[..])?;
                for entry in 0..ENTRIES_PER_BLOCK {
                    let data = &block[entry * ENTRY_SIZE..(entry + 1) * ENTRY_SIZE];
                    let child_ino = get_u32(data, 0);
                    if child_ino == 0 {
                        continue;
                    }
                    let length = data[5] as usize;
                    let name = (length <= NAME_MAX)
                        .then(|| core::str::from_utf8(&data[6..6 + length]).ok())
                        .flatten()
                        .filter(|name| valid_name(name));
                    let child = self.inodes.get(&child_ino).copied();
                    let (Some(name), Some(child)) = (name, child) else {
                        self.damaged = true;
                        continue;
                    };
                    if child.is_dir() {
                        // A directory has one name, or the tree isn't a tree.
                        if seen_directories.insert(child_ino, ()).is_some() || depth + 1 >= MAX_DEPTH {
                            self.damaged = true;
                            continue;
                        }
                    }
                    if self.live_nodes >= MAX_NODES {
                        return Err(FsError::TooManyNodes);
                    }
                    let id = self.nodes.len() as u32;
                    let slot = index * BLOCK as u64 + (entry * ENTRY_SIZE) as u64;
                    self.nodes.push(Some(Node {
                        name: String::from(name),
                        parent: node,
                        ino: child_ino,
                        children: Vec::new(),
                        slot,
                    }));
                    self.live_nodes += 1;
                    self.nodes[node as usize].as_mut().ok_or(FsError::Corrupt)?.children.push(id);
                    if child.is_dir() {
                        queue.push((id, depth + 1));
                    }
                }
            }
        }
        Ok(())
    }

    fn is_allocated(&self, block: u32) -> bool {
        self.allocated
            .get(block as usize / 64)
            .is_none_or(|word| word >> (block % 64) & 1 != 0)
    }

    fn set_allocated(&mut self, block: u32, value: bool) {
        if let Some(word) = self.allocated.get_mut(block as usize / 64) {
            if value {
                *word |= 1 << (block % 64);
            } else {
                *word &= !(1 << (block % 64));
            }
        }
    }

    /// Sets a block's bit in the bitmap, in the open transaction.
    fn mark_bitmap(&mut self, block: u32, used: bool) -> Result<(), FsError> {
        let bits = BLOCK as u32 * 8;
        let home = self.layout.bitmap_start + block / bits;
        let bit = (block % bits) as usize;
        let data = self.meta_mut(home)?;
        if used {
            data[bit / 8] |= 1 << (bit % 8);
        } else {
            data[bit / 8] &= !(1 << (bit % 8));
        }
        Ok(())
    }

    fn allocate_block(&mut self) -> Result<u32, FsError> {
        let (data_start, blocks) = (self.layout.data_start, self.layout.blocks);
        let span = blocks - data_start;
        let mut block = self.next_free.clamp(data_start, blocks - 1);
        for _ in 0..span {
            if !self.is_allocated(block) {
                self.set_allocated(block, true);
                self.mark_bitmap(block, true)?;
                self.next_free = block + 1;
                return Ok(block);
            }
            // A whole word's worth at a time, on a volume that's nearly full.
            if block % 64 == 0 && self.allocated.get(block as usize / 64) == Some(&u64::MAX) {
                block = block.saturating_add(63);
            }
            block += 1;
            if block >= blocks {
                block = data_start;
            }
        }
        Err(FsError::NoSpace)
    }

    /// Frees a block, for use once this transaction has committed.
    fn free_block(&mut self, block: u32) -> Result<(), FsError> {
        if block < self.layout.data_start || block >= self.layout.blocks {
            return Err(FsError::Corrupt);
        }
        self.mark_bitmap(block, false)?;
        // Anything it held is no longer wanted, even if it was changed just now.
        self.open.remove(&block);
        self.pending_free.push(block);
        Ok(())
    }

    fn check_pointer(&self, pointer: u32) -> Result<u32, FsError> {
        if pointer != 0 && (pointer < self.layout.data_start || pointer >= self.layout.blocks) {
            return Err(FsError::Corrupt);
        }
        Ok(pointer)
    }

    fn pointer_in(&self, block: u32, index: usize) -> Result<u32, FsError> {
        let mut data = new_block()?;
        self.read_meta(block, &mut data[..])?;
        self.check_pointer(get_u32(&data[..], index * 4))
    }

    /// The block holding a file's `index`th block of data. Zero for a hole.
    fn block_of(&self, inode: &Inode, index: u64) -> Result<u32, FsError> {
        let mut index = index as usize;
        if index < DIRECT {
            return self.check_pointer(inode.direct[index]);
        }
        index -= DIRECT;
        if index < POINTERS {
            let indirect = self.check_pointer(inode.indirect)?;
            return if indirect == 0 { Ok(0) } else { self.pointer_in(indirect, index) };
        }
        index -= POINTERS;
        if index >= POINTERS * POINTERS {
            return Err(FsError::NoSpace);
        }
        let double = self.check_pointer(inode.double)?;
        if double == 0 {
            return Ok(0);
        }
        let middle = self.pointer_in(double, index / POINTERS)?;
        if middle == 0 {
            return Ok(0);
        }
        self.pointer_in(middle, index % POINTERS)
    }

    fn set_pointer_in(&mut self, block: u32, index: usize, value: u32) -> Result<(), FsError> {
        put_u32(&mut self.meta_mut(block)?[..], index * 4, value);
        Ok(())
    }

    /// A new block of pointers, or a directory's: all zeros, in the open transaction.
    fn allocate_zeroed(&mut self) -> Result<u32, FsError> {
        let block = self.allocate_block()?;
        self.open.insert(block, new_block()?);
        Ok(block)
    }

    /// Makes sure a file has a block at `index`, and returns it. `true` if it's new.
    fn ensure_block(&mut self, inode: &mut Inode, index: u64) -> Result<(u32, bool), FsError> {
        let mut index = index as usize;
        if index < DIRECT {
            let existing = self.check_pointer(inode.direct[index])?;
            if existing != 0 {
                return Ok((existing, false));
            }
            let block = self.allocate_block()?;
            inode.direct[index] = block;
            return Ok((block, true));
        }
        index -= DIRECT;
        if index < POINTERS {
            if self.check_pointer(inode.indirect)? == 0 {
                inode.indirect = self.allocate_zeroed()?;
            }
            let existing = self.pointer_in(inode.indirect, index)?;
            if existing != 0 {
                return Ok((existing, false));
            }
            let block = self.allocate_block()?;
            self.set_pointer_in(inode.indirect, index, block)?;
            return Ok((block, true));
        }
        index -= POINTERS;
        if index >= POINTERS * POINTERS {
            return Err(FsError::NoSpace);
        }
        if self.check_pointer(inode.double)? == 0 {
            inode.double = self.allocate_zeroed()?;
        }
        let mut middle = self.pointer_in(inode.double, index / POINTERS)?;
        if middle == 0 {
            middle = self.allocate_zeroed()?;
            self.set_pointer_in(inode.double, index / POINTERS, middle)?;
        }
        let existing = self.pointer_in(middle, index % POINTERS)?;
        if existing != 0 {
            return Ok((existing, false));
        }
        let block = self.allocate_block()?;
        self.set_pointer_in(middle, index % POINTERS, block)?;
        Ok((block, true))
    }

    /// Frees every block from data block `keep` on, and whatever pointed only at them.
    fn free_from(&mut self, inode: &mut Inode, keep: u64) -> Result<(), FsError> {
        let keep = keep as usize;
        for index in keep.min(DIRECT)..DIRECT {
            let pointer = self.check_pointer(inode.direct[index])?;
            if pointer != 0 {
                self.free_block(pointer)?;
                inode.direct[index] = 0;
            }
        }
        if inode.indirect != 0 {
            let first = keep.saturating_sub(DIRECT);
            if self.free_pointers(inode.indirect, first)? {
                self.free_block(inode.indirect)?;
                inode.indirect = 0;
            }
        }
        if inode.double != 0 {
            let first = keep.saturating_sub(DIRECT + POINTERS);
            let mut data = new_block()?;
            self.read_meta(inode.double, &mut data[..])?;
            let mut all_gone = true;
            for slot in 0..POINTERS {
                let middle = self.check_pointer(get_u32(&data[..], slot * 4))?;
                if middle == 0 {
                    continue;
                }
                let start = slot * POINTERS;
                let from = first.saturating_sub(start);
                if from >= POINTERS {
                    all_gone = false;
                    continue;
                }
                if self.free_pointers(middle, from)? {
                    self.free_block(middle)?;
                    self.set_pointer_in(inode.double, slot, 0)?;
                } else {
                    all_gone = false;
                }
            }
            if all_gone {
                self.free_block(inode.double)?;
                inode.double = 0;
            }
        }
        Ok(())
    }

    /// Frees what a block of pointers holds from `first` on. `true` if nothing's left.
    fn free_pointers(&mut self, block: u32, first: usize) -> Result<bool, FsError> {
        let mut data = new_block()?;
        self.read_meta(block, &mut data[..])?;
        let mut empty = true;
        for slot in 0..POINTERS {
            let pointer = self.check_pointer(get_u32(&data[..], slot * 4))?;
            if pointer == 0 {
                continue;
            }
            if slot >= first {
                self.free_block(pointer)?;
                self.set_pointer_in(block, slot, 0)?;
            } else {
                empty = false;
            }
        }
        Ok(empty)
    }

    fn store_inode(&mut self, ino: u32, inode: Inode) -> Result<(), FsError> {
        let index = ino - 1;
        let home = self.layout.inode_start + index / INODES_PER_BLOCK;
        let at = (index % INODES_PER_BLOCK) as usize * INODE_SIZE;
        let data = self.meta_mut(home)?;
        inode.encode(&mut data[at..at + INODE_SIZE]);
        self.inodes.insert(ino, inode);
        Ok(())
    }

    /// Clears an inode's slot in the table.
    fn clear_inode(&mut self, ino: u32) -> Result<(), FsError> {
        let index = ino - 1;
        let home = self.layout.inode_start + index / INODES_PER_BLOCK;
        let at = (index % INODES_PER_BLOCK) as usize * INODE_SIZE;
        let data = self.meta_mut(home)?;
        data[at..at + INODE_SIZE].fill(0);
        self.inodes.remove(&ino);
        Ok(())
    }

    fn allocate_inode(&mut self) -> Result<u32, FsError> {
        let total = self.layout.inodes;
        let mut ino = self.next_inode.clamp(1, total);
        for _ in 0..total {
            if !self.inodes.contains_key(&ino) {
                self.next_inode = ino + 1;
                return Ok(ino);
            }
            ino = if ino >= total { 1 } else { ino + 1 };
        }
        Err(FsError::NoSpace)
    }

    fn now(&self) -> u64 {
        self.now.and_then(|now| now()).unwrap_or(0)
    }

    fn node_ref(&self, node: NodeId) -> Option<&Node> {
        self.nodes.get(node.0 as usize)?.as_ref()
    }

    fn inode_of(&self, node: NodeId) -> Option<&Inode> {
        self.inodes.get(&self.node_ref(node)?.ino)
    }

    fn writable(&self) -> Result<(), FsError> {
        if self.failed || self.device.is_read_only() {
            return Err(FsError::ReadOnly);
        }
        if self.damaged {
            return Err(FsError::Corrupt);
        }
        Ok(())
    }

    /// Commits if the open transaction has got big. Called between operations, so that one
    /// operation, which only ever touches a few metadata blocks, always fits.
    fn make_room(&mut self) -> Result<(), FsError> {
        if self.open.len() as u32 >= self.transaction_limit() / 2 {
            self.commit()?;
        }
        Ok(())
    }

    fn transaction_limit(&self) -> u32 {
        (self.layout.journal_blocks - 2).min(JOURNAL_MAX_BLOCKS)
    }

    /// Makes the open transaction durable. See the module's documentation for the steps.
    fn commit(&mut self) -> Result<(), FsError> {
        if self.open.is_empty() {
            self.finish_commit();
            return Ok(());
        }
        let result = self.write_transaction();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn write_transaction(&mut self) -> Result<(), FsError> {
        let count = self.open.len() as u32;
        if count > self.transaction_limit() {
            return Err(FsError::NoSpace);
        }
        // 1: what it'll point at.
        self.device.flush()?;
        // 2: the journal.
        let start = self.layout.journal_start;
        let mut descriptor = new_block()?;
        descriptor[..8].copy_from_slice(JOURNAL_DESCRIPTOR);
        put_u64(&mut descriptor[..], 8, self.sequence);
        put_u32(&mut descriptor[..], 16, count);
        for (index, home) in self.open.keys().enumerate() {
            put_u32(&mut descriptor[..], 24 + index * 4, *home);
        }
        let mut crc_input = Vec::new();
        crc_input
            .try_reserve_exact((count as usize + 1) * BLOCK)
            .map_err(|_| FsError::OutOfMemory)?;
        crc_input.extend_from_slice(&descriptor[..]);
        for data in self.open.values() {
            crc_input.extend_from_slice(&data[..]);
        }
        self.write_home(start, &crc_input[..BLOCK])?;
        for index in 0..count as usize {
            let from = (index + 1) * BLOCK;
            self.write_home(start + 1 + index as u32, &crc_input[from..from + BLOCK])?;
        }
        self.device.flush()?;
        // 3: the commit. From here on, it has happened.
        let mut commit = new_block()?;
        commit[..8].copy_from_slice(JOURNAL_COMMIT);
        put_u64(&mut commit[..], 8, self.sequence);
        put_u32(&mut commit[..], 16, count);
        put_u32(&mut commit[..], 20, crc32(&crc_input));
        self.write_home(start + 1 + count, &commit[..])?;
        self.device.flush()?;
        // 4: where they belong.
        let open = core::mem::take(&mut self.open);
        for (home, data) in &open {
            self.write_home(*home, &data[..])?;
        }
        self.device.flush()?;
        // 5: done with the journal.
        let zero = new_block()?;
        self.write_home(start, &zero[..])?;
        self.device.flush()?;
        self.sequence += 1;
        self.finish_commit();
        Ok(())
    }

    /// What was freed in that transaction can be used again.
    fn finish_commit(&mut self) {
        for block in core::mem::take(&mut self.pending_free) {
            self.set_allocated(block, false);
            self.next_free = self.next_free.min(block);
        }
    }

    /// Writes a block of file data in place, and keeps a transaction's copy of it in step.
    fn write_data(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> {
        if let Some(open) = self.open.get_mut(&block) {
            open.copy_from_slice(data);
        }
        self.write_home(block, data)
    }

    fn check_name(&self, parent: NodeId, name: &str) -> Result<(), FsError> {
        if !valid_name(name) {
            return Err(FsError::InvalidName);
        }
        let parent = self.node_ref(parent).ok_or(FsError::NotFound)?;
        if !self.inodes.get(&parent.ino).is_some_and(Inode::is_dir) {
            return Err(FsError::NotDirectory);
        }
        for child in &parent.children {
            if self.nodes[*child as usize].as_ref().is_some_and(|node| node.name == name) {
                return Err(FsError::NameExists);
            }
        }
        Ok(())
    }

    /// Writes one directory entry at a byte position in a directory.
    fn write_entry(&mut self, directory: &Inode, slot: u64, ino: u32, kind: u16, name: &str) -> Result<(), FsError> {
        let home = self.block_of(directory, slot / BLOCK as u64)?;
        if home == 0 {
            return Err(FsError::Corrupt);
        }
        let at = (slot % BLOCK as u64) as usize;
        let data = self.meta_mut(home)?;
        let entry = &mut data[at..at + ENTRY_SIZE];
        entry.fill(0);
        put_u32(entry, 0, ino);
        entry[4] = (kind >> 12) as u8;
        entry[5] = name.len() as u8;
        entry[6..6 + name.len()].copy_from_slice(name.as_bytes());
        Ok(())
    }

    /// Finds a free slot in a directory, growing it if there's none.
    fn free_slot(&mut self, directory_ino: u32) -> Result<u64, FsError> {
        let mut directory = *self.inodes.get(&directory_ino).ok_or(FsError::Corrupt)?;
        let mut data = new_block()?;
        for index in 0..directory.size / BLOCK as u64 {
            let home = self.block_of(&directory, index)?;
            if home == 0 {
                continue;
            }
            self.read_meta(home, &mut data[..])?;
            for entry in 0..ENTRIES_PER_BLOCK {
                if get_u32(&data[entry * ENTRY_SIZE..], 0) == 0 {
                    return Ok(index * BLOCK as u64 + (entry * ENTRY_SIZE) as u64);
                }
            }
        }
        let index = directory.size / BLOCK as u64;
        let (home, _) = self.ensure_block(&mut directory, index)?;
        self.open.insert(home, new_block()?);
        directory.size += BLOCK as u64;
        self.store_inode(directory_ino, directory)?;
        Ok(index * BLOCK as u64)
    }

    fn add_node(&mut self, parent: NodeId, name: &str, ino: u32, slot: u64) -> Result<NodeId, FsError> {
        if self.live_nodes >= MAX_NODES {
            return Err(FsError::TooManyNodes);
        }
        let node = Node {
            name: String::from(name),
            parent: parent.0,
            ino,
            children: Vec::new(),
            slot,
        };
        let id = match self.free_nodes.pop() {
            Some(id) => {
                self.nodes[id as usize] = Some(node);
                id
            }
            None => {
                self.nodes.push(Some(node));
                (self.nodes.len() - 1) as u32
            }
        };
        self.live_nodes += 1;
        self.nodes[parent.0 as usize].as_mut().ok_or(FsError::NotFound)?.children.push(id);
        Ok(NodeId(id))
    }

    fn touch_directory(&mut self, ino: u32) -> Result<(), FsError> {
        let now = self.now();
        let mut directory = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
        directory.modified = now;
        directory.changed = now;
        self.store_inode(ino, directory)
    }

    fn create(&mut self, parent: NodeId, name: &str, mode: u16) -> Result<NodeId, FsError> {
        self.writable()?;
        self.make_room()?;
        self.check_name(parent, name)?;
        let parent_ino = self.node_ref(parent).ok_or(FsError::NotFound)?.ino;
        let ino = self.allocate_inode()?;
        let slot = self.free_slot(parent_ino)?;
        let now = self.now();
        let inode = Inode {
            mode,
            nlink: 1,
            modified: now,
            changed: now,
            created: now,
            ..Inode::default()
        };
        self.store_inode(ino, inode)?;
        let directory = *self.inodes.get(&parent_ino).ok_or(FsError::Corrupt)?;
        self.write_entry(&directory, slot, ino, mode, name)?;
        self.touch_directory(parent_ino)?;
        self.add_node(parent, name, ino, slot)
    }

    /// Adds another name for a file: a hard link.
    pub fn link(&mut self, node: NodeId, new_parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.writable()?;
        self.make_room()?;
        self.check_name(new_parent, name)?;
        let ino = self.node_ref(node).ok_or(FsError::NotFound)?.ino;
        let mut inode = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
        if inode.is_dir() {
            return Err(FsError::IsDirectory);
        }
        let parent_ino = self.node_ref(new_parent).ok_or(FsError::NotFound)?.ino;
        let slot = self.free_slot(parent_ino)?;
        let directory = *self.inodes.get(&parent_ino).ok_or(FsError::Corrupt)?;
        self.write_entry(&directory, slot, ino, inode.mode, name)?;
        inode.nlink += 1;
        inode.changed = self.now();
        self.store_inode(ino, inode)?;
        self.touch_directory(parent_ino)?;
        self.add_node(new_parent, name, ino, slot)
    }

    /// Checks everything against everything else, from what's on the disk. With `repair`,
    /// puts right what can safely be: link counts, the bitmap, and inodes with no name.
    pub fn fsck(&mut self, repair: bool) -> Result<FsckReport, FsError> {
        let mut report = FsckReport {
            total_blocks: self.layout.blocks,
            ..FsckReport::default()
        };
        if self.damaged {
            report.errors.push(String::from("metadata that doesn't read back was found at mount"));
        }
        // Who refers to what.
        let mut references: BTreeMap<u32, u32> = BTreeMap::new();
        for node in self.nodes.iter().flatten() {
            *references.entry(node.ino).or_insert(0) += 1;
        }
        let words = (self.layout.blocks as usize).div_ceil(64);
        let mut used = alloc::vec![0u64; words];
        let mark = |used: &mut [u64], block: u32| -> bool {
            let bit = 1u64 << (block % 64);
            let word = &mut used[block as usize / 64];
            let seen = *word & bit != 0;
            *word |= bit;
            seen
        };
        for block in 0..self.layout.data_start {
            mark(&mut used, block);
        }
        let mut orphans = Vec::new();
        let mut wrong_links = Vec::new();
        let inodes: Vec<(u32, Inode)> = self.inodes.iter().map(|(ino, inode)| (*ino, *inode)).collect();
        for (ino, inode) in inodes {
            let count = references.get(&ino).copied().unwrap_or(0);
            if inode.is_dir() {
                report.directories += 1;
            } else {
                report.files += 1;
            }
            if count == 0 {
                report.errors.push(format!("inode {ino} has no name"));
                orphans.push(ino);
            } else if inode.nlink != count {
                report.errors.push(format!("inode {ino} says {} links, and has {count}", inode.nlink));
                wrong_links.push((ino, count));
            }
            let mut pointers = Vec::new();
            match self.blocks_of(&inode, &mut pointers) {
                Ok(()) => {}
                Err(_) => {
                    report.errors.push(format!("inode {ino} has a block pointer off the volume"));
                    continue;
                }
            }
            let data_blocks = inode.size.div_ceil(BLOCK as u64);
            for (block, index) in &pointers {
                if mark(&mut used, *block) {
                    report.errors.push(format!("block {block} is used twice, by inode {ino} among others"));
                }
                if let Some(index) = index {
                    if *index >= data_blocks {
                        report.errors.push(format!("inode {ino} has a block past its end"));
                    }
                }
            }
        }
        // The bitmap on disk, not the one in memory.
        let mut bitmap = new_block()?;
        let mut wrong_bits = 0u32;
        let bits_per_block = BLOCK as u32 * 8;
        for block in 0..self.layout.blocks {
            if block % bits_per_block == 0 {
                self.read_meta(self.layout.bitmap_start + block / bits_per_block, &mut bitmap[..])?;
            }
            let bit = block % bits_per_block;
            let marked = bitmap[bit as usize / 8] >> (bit % 8) & 1 != 0;
            let in_use = used[block as usize / 64] >> (block % 64) & 1 != 0;
            if in_use {
                report.used_blocks += 1;
            }
            if marked != in_use {
                wrong_bits += 1;
            }
        }
        if wrong_bits > 0 {
            report.errors.push(format!("{wrong_bits} blocks are wrong in the bitmap"));
        }
        // Past the end of the volume has to read as in use, for what allocates to be safe.
        if !repair || self.failed || self.device.is_read_only() {
            return Ok(report);
        }
        for ino in &orphans {
            let mut inode = *self.inodes.get(ino).ok_or(FsError::Corrupt)?;
            if self.free_from(&mut inode, 0).is_ok() {
                self.clear_inode(*ino)?;
                report.repaired += 1;
            }
            self.make_room()?;
        }
        for (ino, count) in &wrong_links {
            let mut inode = *self.inodes.get(ino).ok_or(FsError::Corrupt)?;
            inode.nlink = *count;
            self.store_inode(*ino, inode)?;
            report.repaired += 1;
            self.make_room()?;
        }
        if wrong_bits > 0 {
            // Walk again, as freeing orphans will have changed what's used.
            self.commit()?;
            self.rebuild_bitmap()?;
            report.repaired += 1;
        }
        self.commit()?;
        self.device.flush()?;
        Ok(report)
    }

    /// Every block a file holds: `(block, Some(data index))` for data, or `None` for pointers.
    fn blocks_of(&self, inode: &Inode, out: &mut Vec<(u32, Option<u64>)>) -> Result<(), FsError> {
        for (index, pointer) in inode.direct.iter().enumerate() {
            if self.check_pointer(*pointer)? != 0 {
                out.push((*pointer, Some(index as u64)));
            }
        }
        if self.check_pointer(inode.indirect)? != 0 {
            out.push((inode.indirect, None));
            let mut data = new_block()?;
            self.read_meta(inode.indirect, &mut data[..])?;
            for slot in 0..POINTERS {
                let pointer = self.check_pointer(get_u32(&data[..], slot * 4))?;
                if pointer != 0 {
                    out.push((pointer, Some((DIRECT + slot) as u64)));
                }
            }
        }
        if self.check_pointer(inode.double)? != 0 {
            out.push((inode.double, None));
            let mut outer = new_block()?;
            let mut data = new_block()?;
            self.read_meta(inode.double, &mut outer[..])?;
            for slot in 0..POINTERS {
                let middle = self.check_pointer(get_u32(&outer[..], slot * 4))?;
                if middle == 0 {
                    continue;
                }
                out.push((middle, None));
                self.read_meta(middle, &mut data[..])?;
                for inner in 0..POINTERS {
                    let pointer = self.check_pointer(get_u32(&data[..], inner * 4))?;
                    if pointer != 0 {
                        out.push((pointer, Some((DIRECT + POINTERS + slot * POINTERS + inner) as u64)));
                    }
                }
            }
        }
        Ok(())
    }

    fn rebuild_bitmap(&mut self) -> Result<(), FsError> {
        let words = (self.layout.blocks as usize).div_ceil(64);
        self.allocated = alloc::vec![0u64; words];
        for block in 0..self.layout.data_start {
            self.set_allocated(block, true);
        }
        let inodes: Vec<Inode> = self.inodes.values().copied().collect();
        for inode in &inodes {
            let mut pointers = Vec::new();
            self.blocks_of(inode, &mut pointers)?;
            for (block, _) in pointers {
                self.set_allocated(block, true);
            }
        }
        let bits_per_block = BLOCK as u32 * 8;
        for index in 0..self.layout.bitmap_blocks {
            let first = index * bits_per_block;
            let mut image = new_block()?;
            for bit in 0..bits_per_block {
                let block = first + bit;
                if block >= self.layout.blocks || self.is_allocated(block) {
                    image[bit as usize / 8] |= 1 << (bit % 8);
                }
            }
            self.open.insert(self.layout.bitmap_start + index, image);
            self.make_room()?;
        }
        self.next_free = self.layout.data_start;
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != "."
        && name != ".."
        && !name.bytes().any(|byte| byte == b'/' || byte == 0)
}

impl<D: WritableBlockDevice> FileSystem for Native<D> {
    fn node_count(&self) -> usize {
        self.live_nodes
    }

    fn node_id_limit(&self) -> usize {
        self.nodes.len()
    }

    fn is_damaged(&self) -> bool {
        self.damaged
    }

    fn kind(&self, node: NodeId) -> Option<EntryKind> {
        self.inode_of(node).map(Inode::kind)
    }

    fn name(&self, node: NodeId) -> Option<&str> {
        self.node_ref(node).map(|node| node.name.as_str())
    }

    fn parent(&self, node: NodeId) -> Option<NodeId> {
        if node == NodeId::ROOT {
            return None;
        }
        self.node_ref(node).map(|entry| NodeId(entry.parent))
    }

    fn size(&self, node: NodeId) -> usize {
        match self.inode_of(node) {
            Some(inode) if !inode.is_dir() => inode.size as usize,
            _ => 0,
        }
    }

    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize {
        self.try_read(node, offset, out).unwrap_or(0)
    }

    fn try_read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        let inode = self.inode_of(node).ok_or(FsError::NotFound)?;
        if inode.is_dir() {
            return Err(FsError::NotFile);
        }
        let size = inode.size as usize;
        if offset >= size || out.is_empty() {
            return Ok(0);
        }
        let total = out.len().min(size - offset);
        let mut done = 0;
        let mut block = new_block()?;
        while done < total {
            let at = offset + done;
            let inside = at % BLOCK;
            let count = (BLOCK - inside).min(total - done);
            let home = self.block_of(inode, (at / BLOCK) as u64)?;
            if home == 0 {
                out[done..done + count].fill(0);
            } else {
                self.read_meta(home, &mut block[..])?;
                out[done..done + count].copy_from_slice(&block[inside..inside + count]);
            }
            done += count;
        }
        Ok(done)
    }

    fn child_count(&self, node: NodeId) -> usize {
        self.node_ref(node).map_or(0, |node| node.children.len())
    }

    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId> {
        self.node_ref(node)?.children.get(index).map(|id| NodeId(*id))
    }

    fn attributes(&self, node: NodeId) -> u8 {
        self.inode_of(node).map_or(0, |inode| inode.attributes)
    }

    fn times(&self, node: NodeId) -> NodeTimes {
        let time = |value: u64| (value != 0).then_some(value);
        self.inode_of(node).map_or(NodeTimes::NONE, |inode| NodeTimes {
            created: time(inode.created),
            modified: time(inode.modified),
            accessed: time(inode.modified),
        })
    }

    fn is_read_only(&self) -> bool {
        self.damaged || self.failed || self.device.is_read_only()
    }

    fn stats(&self) -> VolumeStats {
        let used: u32 = self.allocated.iter().map(|word| word.count_ones()).sum();
        // Bits past the end of the volume in the last word are set too.
        let spare = (self.allocated.len() as u32 * 64).saturating_sub(self.layout.blocks);
        VolumeStats {
            block_size: BLOCK as u64,
            total_blocks: u64::from(self.layout.blocks),
            free_blocks: u64::from(self.layout.blocks.saturating_sub(used.saturating_sub(spare))),
            read_only: self.is_read_only(),
            kind: "feos-native",
        }
    }
}

impl<D: WritableBlockDevice> WritableFileSystem for Native<D> {
    fn create_file(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.create(parent, name, S_IFREG | 0o644)
    }

    fn create_dir(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.create(parent, name, S_IFDIR | 0o755)
    }

    fn remove(&mut self, node: NodeId) -> Result<(), FsError> {
        self.writable()?;
        self.make_room()?;
        if node == NodeId::ROOT {
            return Err(FsError::Busy);
        }
        let entry = self.node_ref(node).ok_or(FsError::NotFound)?;
        if !entry.children.is_empty() {
            return Err(FsError::NotEmpty);
        }
        let (ino, parent, slot) = (entry.ino, entry.parent, entry.slot);
        let parent_ino = self.nodes[parent as usize].as_ref().ok_or(FsError::Corrupt)?.ino;
        let directory = *self.inodes.get(&parent_ino).ok_or(FsError::Corrupt)?;
        self.write_entry(&directory, slot, 0, 0, "")?;
        let mut inode = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
        inode.nlink = inode.nlink.saturating_sub(1);
        if inode.nlink == 0 {
            self.free_from(&mut inode, 0)?;
            self.clear_inode(ino)?;
        } else {
            inode.changed = self.now();
            self.store_inode(ino, inode)?;
        }
        self.touch_directory(parent_ino)?;
        self.nodes[parent as usize]
            .as_mut()
            .ok_or(FsError::Corrupt)?
            .children
            .retain(|child| *child != node.0);
        self.nodes[node.0 as usize] = None;
        self.free_nodes.push(node.0);
        self.live_nodes -= 1;
        Ok(())
    }

    fn rename(&mut self, node: NodeId, new_parent: NodeId, new_name: &str) -> Result<(), FsError> {
        self.writable()?;
        self.make_room()?;
        if node == NodeId::ROOT {
            return Err(FsError::Busy);
        }
        // Not into itself.
        let mut walk = new_parent;
        loop {
            if walk == node {
                return Err(FsError::InvalidName);
            }
            match self.parent(walk) {
                Some(parent) => walk = parent,
                None => break,
            }
        }
        self.check_name(new_parent, new_name)?;
        let entry = self.node_ref(node).ok_or(FsError::NotFound)?;
        let (ino, old_parent, old_slot) = (entry.ino, entry.parent, entry.slot);
        let mode = self.inodes.get(&ino).ok_or(FsError::Corrupt)?.mode;
        let old_parent_ino = self.nodes[old_parent as usize].as_ref().ok_or(FsError::Corrupt)?.ino;
        let new_parent_ino = self.node_ref(new_parent).ok_or(FsError::NotFound)?.ino;
        let slot = self.free_slot(new_parent_ino)?;
        // Both in one transaction: a crash leaves the one name or the other.
        let directory = *self.inodes.get(&new_parent_ino).ok_or(FsError::Corrupt)?;
        self.write_entry(&directory, slot, ino, mode, new_name)?;
        let directory = *self.inodes.get(&old_parent_ino).ok_or(FsError::Corrupt)?;
        self.write_entry(&directory, old_slot, 0, 0, "")?;
        self.touch_directory(old_parent_ino)?;
        self.touch_directory(new_parent_ino)?;
        self.nodes[old_parent as usize]
            .as_mut()
            .ok_or(FsError::Corrupt)?
            .children
            .retain(|child| *child != node.0);
        self.nodes[new_parent.0 as usize].as_mut().ok_or(FsError::Corrupt)?.children.push(node.0);
        let entry = self.nodes[node.0 as usize].as_mut().ok_or(FsError::Corrupt)?;
        entry.parent = new_parent.0;
        entry.slot = slot;
        entry.name = String::from(new_name);
        Ok(())
    }

    fn write(&mut self, node: NodeId, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        self.writable()?;
        let ino = self.node_ref(node).ok_or(FsError::NotFound)?.ino;
        let mut inode = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
        if inode.is_dir() {
            return Err(FsError::IsDirectory);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let mut done = 0;
        let mut buffer = new_block()?;
        while done < data.len() {
            // Between blocks is between operations too: the size written so far is in
            // the inode, and a commit here is a shorter file, not a damaged one.
            if self.open.len() as u32 >= self.transaction_limit() / 2 {
                self.store_inode(ino, inode)?;
                self.commit()?;
            }
            let at = offset + done;
            let inside = at % BLOCK;
            let count = (BLOCK - inside).min(data.len() - done);
            let (home, fresh) = match self.ensure_block(&mut inode, (at / BLOCK) as u64) {
                Ok(found) => found,
                Err(error) if done > 0 && error == FsError::NoSpace => break,
                Err(error) => return Err(error),
            };
            if count == BLOCK {
                buffer.copy_from_slice(&data[done..done + BLOCK]);
            } else {
                if fresh {
                    buffer.fill(0);
                } else {
                    self.read_meta(home, &mut buffer[..])?;
                }
                buffer[inside..inside + count].copy_from_slice(&data[done..done + count]);
            }
            self.write_data(home, &buffer[..])?;
            done += count;
            inode.size = inode.size.max((offset + done) as u64);
        }
        let now = self.now();
        inode.modified = now;
        inode.changed = now;
        self.store_inode(ino, inode)?;
        Ok(done)
    }

    fn set_len(&mut self, node: NodeId, size: usize) -> Result<(), FsError> {
        self.writable()?;
        self.make_room()?;
        let ino = self.node_ref(node).ok_or(FsError::NotFound)?.ino;
        let mut inode = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
        if inode.is_dir() {
            return Err(FsError::IsDirectory);
        }
        let size = size as u64;
        if size < inode.size {
            self.free_from(&mut inode, size.div_ceil(BLOCK as u64))?;
            // What's past the new end has to read as zeros if the file grows again. In the
            // journal, so that it goes with the size.
            let inside = (size % BLOCK as u64) as usize;
            if inside != 0 {
                let home = self.block_of(&inode, size / BLOCK as u64)?;
                if home != 0 {
                    self.meta_mut(home)?[inside..].fill(0);
                }
            }
        }
        // Growing needs nothing: past the end is a hole, and a hole reads as zeros.
        inode.size = size;
        let now = self.now();
        inode.modified = now;
        inode.changed = now;
        self.store_inode(ino, inode)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        if self.failed {
            return Err(FsError::Io);
        }
        if self.device.is_read_only() {
            return Ok(());
        }
        self.commit()?;
        self.device.flush()
    }

    fn set_attributes(&mut self, node: NodeId, attributes: u8) -> Result<(), FsError> {
        self.writable()?;
        self.make_room()?;
        let ino = self.node_ref(node).ok_or(FsError::NotFound)?.ino;
        let mut inode = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
        inode.attributes = attributes & attr::SETTABLE;
        // The VFS marks a link with this bit, over a file that starts with its magic.
        if !inode.is_dir() && attributes & attr::SYSTEM != 0 {
            let mut head = [0u8; SYMLINK_MAGIC.len()];
            if self.try_read(node, 0, &mut head)? == head.len() && head == *SYMLINK_MAGIC {
                inode.mode = (inode.mode & !S_IFMT) | S_IFLNK;
            }
        }
        inode.changed = self.now();
        self.store_inode(ino, inode)
    }

    fn set_times(&mut self, node: NodeId, times: NodeTimes) -> Result<(), FsError> {
        self.writable()?;
        self.make_room()?;
        let ino = self.node_ref(node).ok_or(FsError::NotFound)?.ino;
        let mut inode = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
        if let Some(created) = times.created {
            inode.created = created;
        }
        if let Some(modified) = times.modified {
            inode.modified = modified;
        }
        inode.changed = self.now();
        self.store_inode(ino, inode)
    }

    fn set_time_source(&mut self, now: TimeSource) {
        self.now = Some(now);
    }

    fn check(&mut self) -> Result<CheckReport, FsError> {
        Ok(CheckReport {
            was_dirty: self.replayed,
            ..CheckReport::default()
        })
    }

    fn posix(&self, node: NodeId) -> Option<PosixMeta> {
        let entry = self.node_ref(node)?;
        let inode = self.inodes.get(&entry.ino)?;
        let nlink = if inode.is_dir() {
            let subdirectories = entry
                .children
                .iter()
                .filter(|child| {
                    self.nodes[**child as usize]
                        .as_ref()
                        .and_then(|node| self.inodes.get(&node.ino))
                        .is_some_and(Inode::is_dir)
                })
                .count();
            2 + subdirectories as u32
        } else {
            inode.nlink
        };
        Some(PosixMeta {
            mode: inode.mode,
            uid: inode.uid,
            gid: inode.gid,
            nlink,
            ino: u64::from(entry.ino),
        })
    }

    fn set_mode(&mut self, node: NodeId, mode: u16) -> Result<(), FsError> {
        self.writable()?;
        self.make_room()?;
        let ino = self.node_ref(node).ok_or(FsError::NotFound)?.ino;
        let mut inode = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
        inode.mode = (inode.mode & S_IFMT) | (mode & MODE_BITS);
        inode.changed = self.now();
        self.store_inode(ino, inode)
    }

    fn set_owner(&mut self, node: NodeId, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
        self.writable()?;
        self.make_room()?;
        let ino = self.node_ref(node).ok_or(FsError::NotFound)?.ino;
        let mut inode = *self.inodes.get(&ino).ok_or(FsError::Corrupt)?;
        if let Some(uid) = uid {
            inode.uid = uid;
        }
        if let Some(gid) = gid {
            inode.gid = gid;
        }
        inode.changed = self.now();
        self.store_inode(ino, inode)
    }

    fn hard_link(&mut self, node: NodeId, new_parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.link(node, new_parent, name)
    }

    fn fsck(&mut self, repair: bool) -> Result<FsckReport, FsError> {
        Native::fsck(self, repair)
    }
}
