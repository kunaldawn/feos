//! Byte-addressed access on top of a [`BlockDevice`].
//!
//! Filesystem sectors are not necessarily the device's logical blocks: a real
//! disk usually exposes 512-byte blocks while a filesystem may use 4096-byte
//! sectors. This layer hides the difference and provides checked little-endian
//! scalar reads used by both on-disk format parsers.

use crate::block::{BlockDevice, WritableBlockDevice, MAX_BLOCK_SIZE};
use crate::FsError;

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
        })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
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
                let mut scratch = [0u8; MAX_BLOCK_SIZE];
                let buffer = &mut scratch[..self.block_size];
                self.device.read_block(block, buffer)?;
                out[done..done + chunk].copy_from_slice(&buffer[within..within + chunk]);
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
                self.device.write_block(block, &data[done..done + chunk])?;
            } else {
                let mut scratch = [0u8; MAX_BLOCK_SIZE];
                let buffer = &mut scratch[..self.block_size];
                self.device.read_block(block, buffer)?;
                buffer[within..within + chunk].copy_from_slice(&data[done..done + chunk]);
                self.device.write_block(block, buffer)?;
            }
            done += chunk;
        }
        Ok(())
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

    Err(FsError::UnsupportedVolume)
}
