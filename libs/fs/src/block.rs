//! Block devices and RAM-backed implementations for disk images.

use alloc::vec::Vec;

use crate::FsError;

/// Largest logical block size the volume layer can stage in a stack buffer.
pub const MAX_BLOCK_SIZE: usize = 8192;

/// A random-access block device.
///
/// Implementations may be backed by a virtio block device, an SD card, a
/// memory-mapped disk image, or anything else that can return fixed-size
/// blocks. All filesystem code is generic over this trait, so the same FAT32
/// and exFAT drivers run on every board.
pub trait BlockDevice: Sync {
    /// Logical block size in bytes. Must be a power of two no larger than
    /// [`MAX_BLOCK_SIZE`].
    fn block_size(&self) -> usize;

    /// Number of whole blocks available.
    fn block_count(&self) -> u64;

    /// Reads one block into `out`.
    ///
    /// `out` must be exactly [`block_size`](BlockDevice::block_size) bytes.
    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError>;
}

impl<T: BlockDevice + ?Sized> BlockDevice for &T {
    fn block_size(&self) -> usize {
        (**self).block_size()
    }

    fn block_count(&self) -> u64 {
        (**self).block_count()
    }

    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        (**self).read_block(block, out)
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for &mut T {
    fn block_size(&self) -> usize {
        (**self).block_size()
    }

    fn block_count(&self) -> u64 {
        (**self).block_count()
    }

    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        (**self).read_block(block, out)
    }
}

/// A block device that can also be written and flushed.
///
/// The writable filesystem backends take `&mut self` for mutations, so a
/// device only needs interior mutability if the caller also wants to share it
/// concurrently.
pub trait WritableBlockDevice: BlockDevice {
    /// Writes one block. `data` must be exactly the device block size.
    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError>;

    /// Flushes cached writes to the medium.
    fn flush(&mut self) -> Result<(), FsError> {
        Ok(())
    }
}

impl<T: WritableBlockDevice + ?Sized> WritableBlockDevice for &mut T {
    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError> {
        (**self).write_block(block, data)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        (**self).flush()
    }
}

/// A block device backed by an in-memory disk image.
///
/// The image may be a compiled-in FAT32/exFAT file or a test fixture:
///
/// ```no_run
/// use feos_fs::{mount_image, MemoryBlockDevice};
///
/// static DISK: &[u8] = &[];
/// let fs = mount_image(DISK).unwrap();
/// ```
#[derive(Clone, Copy, Debug)]
pub struct MemoryBlockDevice<'a> {
    data: &'a [u8],
    block_size: usize,
}

impl<'a> MemoryBlockDevice<'a> {
    /// Wraps `data` with the usual 512-byte logical sector size.
    pub const fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            block_size: 512,
        }
    }

    /// Wraps `data` using an explicit logical block size.
    pub const fn with_block_size(data: &'a [u8], block_size: usize) -> Self {
        Self { data, block_size }
    }

    /// The wrapped image.
    pub const fn data(self) -> &'a [u8] {
        self.data
    }
}

impl BlockDevice for MemoryBlockDevice<'_> {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        if self.block_size == 0 {
            0
        } else {
            (self.data.len() / self.block_size) as u64
        }
    }

    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        if out.len() != self.block_size {
            return Err(FsError::InvalidBuffer);
        }
        let offset = block
            .checked_mul(self.block_size as u64)
            .ok_or(FsError::OutOfBounds)?;
        let start = usize::try_from(offset).map_err(|_| FsError::OutOfBounds)?;
        let end = start
            .checked_add(self.block_size)
            .ok_or(FsError::OutOfBounds)?;
        let source = self.data.get(start..end).ok_or(FsError::OutOfBounds)?;
        out.copy_from_slice(source);
        Ok(())
    }
}

/// A writable block device backed by a mutable disk image.
///
/// ```no_run
/// use feos_fs::{Fat32, MemoryBlockDeviceMut, NodeId, WritableFileSystem};
///
/// let mut image = std::fs::read("disk.img").unwrap();
/// let mut fs = Fat32::mount(MemoryBlockDeviceMut::new(&mut image)).unwrap();
/// let file = fs.create_file(NodeId::ROOT, "hello.txt").unwrap();
/// fs.write(file, 0, b"hello\n").unwrap();
/// fs.flush().unwrap();
/// ```
#[derive(Debug)]
pub struct MemoryBlockDeviceMut<'a> {
    data: &'a mut [u8],
    block_size: usize,
}

impl<'a> MemoryBlockDeviceMut<'a> {
    /// Wraps `data` with the usual 512-byte logical sector size.
    pub const fn new(data: &'a mut [u8]) -> Self {
        Self {
            data,
            block_size: 512,
        }
    }

    /// Wraps `data` using an explicit logical block size.
    pub const fn with_block_size(data: &'a mut [u8], block_size: usize) -> Self {
        Self { data, block_size }
    }

    /// The wrapped image.
    pub const fn data(&self) -> &[u8] {
        self.data
    }

    /// The wrapped image, mutably.
    pub const fn data_mut(&mut self) -> &mut [u8] {
        self.data
    }
}

impl BlockDevice for MemoryBlockDeviceMut<'_> {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        if self.block_size == 0 {
            0
        } else {
            (self.data.len() / self.block_size) as u64
        }
    }

    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        if out.len() != self.block_size {
            return Err(FsError::InvalidBuffer);
        }
        let offset = block
            .checked_mul(self.block_size as u64)
            .ok_or(FsError::OutOfBounds)?;
        let start = usize::try_from(offset).map_err(|_| FsError::OutOfBounds)?;
        let end = start
            .checked_add(self.block_size)
            .ok_or(FsError::OutOfBounds)?;
        let source = self.data.get(start..end).ok_or(FsError::OutOfBounds)?;
        out.copy_from_slice(source);
        Ok(())
    }
}

impl WritableBlockDevice for MemoryBlockDeviceMut<'_> {
    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError> {
        if data.len() != self.block_size {
            return Err(FsError::InvalidBuffer);
        }
        let offset = block
            .checked_mul(self.block_size as u64)
            .ok_or(FsError::OutOfBounds)?;
        let start = usize::try_from(offset).map_err(|_| FsError::OutOfBounds)?;
        let end = start
            .checked_add(self.block_size)
            .ok_or(FsError::OutOfBounds)?;
        let target = self.data.get_mut(start..end).ok_or(FsError::OutOfBounds)?;
        target.copy_from_slice(data);
        Ok(())
    }
}

/// A heap-backed RAM disk.
///
/// Useful for a writable scratch volume on boards without a block driver, and
/// for tests that want to inspect the image after unmounting.
#[derive(Debug)]
pub struct RamDisk {
    data: Vec<u8>,
    block_size: usize,
}

impl RamDisk {
    /// Creates a zero-filled RAM disk of `size` bytes.
    pub fn new(size: usize, block_size: usize) -> Result<Self, FsError> {
        if block_size == 0
            || block_size > MAX_BLOCK_SIZE
            || !block_size.is_power_of_two()
            || size % block_size != 0
        {
            return Err(FsError::UnsupportedBlockSize);
        }
        Ok(Self {
            data: alloc::vec![0u8; size],
            block_size,
        })
    }

    /// Wraps an existing image in a RAM disk.
    pub fn from_bytes(data: Vec<u8>, block_size: usize) -> Result<Self, FsError> {
        if block_size == 0 || block_size > MAX_BLOCK_SIZE || !block_size.is_power_of_two() {
            return Err(FsError::UnsupportedBlockSize);
        }
        Ok(Self { data, block_size })
    }

    /// The raw image.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the disk and returns the raw image.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

impl BlockDevice for RamDisk {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        (self.data.len() / self.block_size) as u64
    }

    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        if out.len() != self.block_size {
            return Err(FsError::InvalidBuffer);
        }
        let offset = block
            .checked_mul(self.block_size as u64)
            .ok_or(FsError::OutOfBounds)?;
        let start = usize::try_from(offset).map_err(|_| FsError::OutOfBounds)?;
        let end = start
            .checked_add(self.block_size)
            .ok_or(FsError::OutOfBounds)?;
        let source = self.data.get(start..end).ok_or(FsError::OutOfBounds)?;
        out.copy_from_slice(source);
        Ok(())
    }
}

impl WritableBlockDevice for RamDisk {
    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError> {
        if data.len() != self.block_size {
            return Err(FsError::InvalidBuffer);
        }
        let offset = block
            .checked_mul(self.block_size as u64)
            .ok_or(FsError::OutOfBounds)?;
        let start = usize::try_from(offset).map_err(|_| FsError::OutOfBounds)?;
        let end = start
            .checked_add(self.block_size)
            .ok_or(FsError::OutOfBounds)?;
        let target = self.data.get_mut(start..end).ok_or(FsError::OutOfBounds)?;
        target.copy_from_slice(data);
        Ok(())
    }
}
