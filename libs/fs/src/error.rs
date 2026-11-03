//! Errors returned while probing or mounting a filesystem.

use core::fmt;

/// Why a volume could not be mounted or read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FsError {
    /// The block device reported a read failure.
    Io,
    /// A read or write fell outside the device.
    OutOfBounds,
    /// The supplied buffer did not match the device block size.
    InvalidBuffer,
    /// The block device uses an unsupported logical block size.
    UnsupportedBlockSize,
    /// The boot sector is missing or malformed.
    InvalidBootSector,
    /// The volume is a filesystem this crate does not implement.
    UnsupportedVolume,
    /// On-disk metadata is inconsistent (bad chain, bad directory entry, ...).
    Corrupt,
    /// The directory tree is too large to represent in memory.
    TooManyNodes,
    /// The volume has no free clusters left.
    NoSpace,
    /// A file or directory with that name already exists.
    NameExists,
    /// The requested entry does not exist.
    NotFound,
    /// A component that must be a directory is a file.
    NotDirectory,
    /// The node is not a file.
    NotFile,
    /// A directory still contains entries and cannot be removed.
    NotEmpty,
    /// The supplied name is not valid for the target filesystem.
    InvalidName,
    /// The operation is not supported by this backend.
    Unsupported,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Io => "block device I/O error",
            Self::OutOfBounds => "read outside the block device",
            Self::InvalidBuffer => "buffer does not match the block size",
            Self::UnsupportedBlockSize => "unsupported block size",
            Self::InvalidBootSector => "invalid boot sector",
            Self::UnsupportedVolume => "unsupported filesystem",
            Self::Corrupt => "corrupt filesystem metadata",
            Self::TooManyNodes => "too many filesystem entries",
            Self::NoSpace => "no space left on the volume",
            Self::NameExists => "name already exists",
            Self::NotFound => "entry not found",
            Self::NotDirectory => "not a directory",
            Self::NotFile => "not a file",
            Self::NotEmpty => "directory is not empty",
            Self::InvalidName => "invalid file name",
            Self::Unsupported => "operation not supported",
        };
        f.write_str(message)
    }
}

#[cfg(test)]
impl std::error::Error for FsError {}
