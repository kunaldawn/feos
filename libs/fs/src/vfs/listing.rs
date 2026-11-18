//! A directory's listing as a program reads it: one snapshot, taken whole, walked by
//! position.
//!
//! The tree returns whole listings, sorted by name, so a program's `GETDENTS` takes one
//! into its open directory and goes through it a piece at a time; `.` and `..` come first.
//! A position (a cookie, to `SEEK`) is an entry's index, so a cookie from one snapshot
//! means the same entry in another of the same unchanged directory. Entries added or
//! removed after a snapshot aren't seen until the next, which POSIX allows. [`ORPHANS`]
//! is never listed: the tree's listing of a mount's root leaves it out.
//!
//! [`ORPHANS`]: super::ORPHANS

use alloc::string::String;
use alloc::vec::Vec;

use super::{FileKind, VfsTree};
use crate::FsError;

/// One entry of a [`Snapshot`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Listed {
    pub name: String,
    pub ino: u64,
    pub kind: FileKind,
    /// A device node, which a listing calls a character device.
    pub device: bool,
}

/// A directory's entries as they were when it was taken, `.` and `..` first.
#[derive(Debug, Default)]
pub struct Snapshot {
    entries: Vec<Listed>,
    /// Heap the names and entries take, about.
    bytes: usize,
}

impl Snapshot {
    /// The listing of the directory whose canonical path is `directory`.
    pub fn take(tree: &dyn VfsTree, directory: &str) -> Result<Self, FsError> {
        let own = tree.resolve_path(directory, true)?;
        if own.stat.kind != FileKind::Directory {
            return Err(FsError::NotDirectory);
        }
        let parent = match own.canonical.rsplit_once('/') {
            Some(("", _)) | None => "/",
            Some((parent, _)) => parent,
        };
        let parent_ino = if own.canonical == "/" { own.stat.ino } else { tree.resolve_path(parent, true)?.stat.ino };
        let listing = tree.read_dir(&own.canonical)?;
        let mut entries = Vec::new();
        entries.try_reserve_exact(listing.len() + 2).map_err(|_| FsError::OutOfMemory)?;
        let mut bytes = (listing.len() + 2) * core::mem::size_of::<Listed>();
        for (name, ino) in [(".", own.stat.ino), ("..", parent_ino)] {
            entries.push(Listed { name: copy(name)?, ino, kind: FileKind::Directory, device: false });
        }
        for entry in listing {
            bytes += entry.name.len();
            entries.push(Listed { ino: entry.stat.ino, kind: entry.stat.kind, device: entry.stat.device.is_some(), name: entry.name });
        }
        Ok(Self { entries, bytes })
    }

    /// Entries, `.` and `..` included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry at position `index`.
    pub fn get(&self, index: usize) -> Option<&Listed> {
        self.entries.get(index)
    }

    /// Heap it takes, about: for a caller that charges it to someone.
    pub fn heap_bytes(&self) -> usize {
        self.bytes
    }
}

fn copy(text: &str) -> Result<String, FsError> {
    let mut out = String::new();
    out.try_reserve_exact(text.len()).map_err(|_| FsError::OutOfMemory)?;
    out.push_str(text);
    Ok(out)
}
