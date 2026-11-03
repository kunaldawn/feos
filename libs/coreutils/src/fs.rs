//! A writable in-memory overlay on top of the board's read-only filesystem.
//!
//! The board compiles a small read-only image into the kernel (`feos-fs`).
//! A real terminal needs `mkdir`, `cp`, `mv`, `rm`, output redirection and
//! friends, so this module keeps every mutation in a `BTreeMap` overlay and
//! leaves the immutable base image untouched. Reads fall through to the base,
//! writes are captured in RAM, and deletes are recorded as tombstones.
//!
//! The implementation is deliberately lexical: there are no symlinks, hard
//! links, permissions or timestamps yet. Paths are normalised relative to a
//! current working directory and never escape the root.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_fs::FileSystem;

/// What a filesystem entry is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
}

/// Metadata for one path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    /// File or directory.
    pub kind: Kind,
    /// Size in bytes (zero for directories).
    pub size: usize,
}

/// One directory entry produced by [`Vfs::read_dir`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DirEntry {
    /// Final path component.
    pub name: String,
    /// File or directory.
    pub kind: Kind,
    /// Size in bytes (zero for directories).
    pub size: usize,
}

/// Errors shared by the virtual filesystem and the utilities.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsError {
    /// The path does not exist.
    NotFound,
    /// A path component is not a directory.
    NotDir,
    /// The path is a directory where a file was expected.
    IsDir,
    /// The path already exists.
    Exists,
    /// A directory is not empty.
    NotEmpty,
    /// The path is malformed or invalid for the operation.
    InvalidPath,
    /// The operation is not supported by this filesystem.
    NotSupported,
}

impl FsError {
    /// A short human-readable description.
    pub const fn text(self) -> &'static str {
        match self {
            Self::NotFound => "No such file or directory",
            Self::NotDir => "Not a directory",
            Self::IsDir => "Is a directory",
            Self::Exists => "File exists",
            Self::NotEmpty => "Directory not empty",
            Self::InvalidPath => "Invalid path",
            Self::NotSupported => "Operation not supported",
        }
    }
}

/// A writable filesystem view used by the core utilities.
pub trait Vfs {
    /// The current working directory, always absolute.
    fn cwd(&self) -> &str;

    /// Changes the working directory.
    fn chdir(&mut self, path: &str) -> Result<(), FsError>;

    /// Returns metadata for `path`.
    fn meta(&self, path: &str) -> Result<Meta, FsError>;

    /// Reads a whole file into memory.
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, FsError>;

    /// Writes a whole file. When `append` is true the existing bytes are kept.
    fn write_file(&mut self, path: &str, data: &[u8], append: bool) -> Result<(), FsError>;

    /// Creates a single directory.
    fn create_dir(&mut self, path: &str) -> Result<(), FsError>;

    /// Removes a regular file.
    fn remove_file(&mut self, path: &str) -> Result<(), FsError>;

    /// Removes an empty directory.
    fn remove_dir(&mut self, path: &str) -> Result<(), FsError>;

    /// Renames `from` to `to`. The destination must not exist.
    fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError>;

    /// Lists a directory, sorted by name.
    fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError>;

    /// Resolves `path` against the current working directory.
    fn resolve(&self, path: &str) -> String {
        normalize(self.cwd(), path)
    }

    /// Returns `true` when `path` exists.
    fn exists(&self, path: &str) -> bool {
        self.meta(path).is_ok()
    }

    /// Returns `true` when `path` is a directory.
    fn is_dir(&self, path: &str) -> bool {
        self.meta(path)
            .map(|meta| meta.kind == Kind::Dir)
            .unwrap_or(false)
    }
}

/// Normalises `path` against `cwd` into an absolute, slash-separated path.
///
/// `..` never walks above `/`; duplicate slashes and `.` components vanish.
pub fn normalize(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();

    if !path.starts_with('/') {
        for part in cwd.split('/') {
            if !part.is_empty() {
                parts.push(part);
            }
        }
    }

    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    let mut result = String::new();
    for part in parts {
        result.push('/');
        result.push_str(part);
    }
    if result.is_empty() {
        result.push('/');
    }
    result
}

/// Returns the parent directory of an absolute, normalised path.
pub fn parent(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &path[..index],
    }
}

/// Returns the final component of an absolute, normalised path.
pub fn base_name(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    path.rsplit('/').next().unwrap_or(path)
}

/// Joins a directory and a child name.
pub fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        let mut result = String::from("/");
        result.push_str(name);
        result
    } else {
        let mut result = String::from(dir);
        result.push('/');
        result.push_str(name);
        result
    }
}

#[derive(Clone, Debug)]
enum Node {
    File(Vec<u8>),
    Dir,
    Tombstone,
}

/// An overlay filesystem with an optional read-only base image.
pub struct OverlayFs {
    base: Option<&'static dyn FileSystem>,
    nodes: BTreeMap<String, Node>,
    cwd: String,
}

impl OverlayFs {
    /// Creates a filesystem backed by `base` when one is available.
    pub fn new(base: Option<&'static dyn FileSystem>) -> Self {
        Self {
            base,
            nodes: BTreeMap::new(),
            cwd: String::from("/"),
        }
    }

    /// Creates a purely in-memory filesystem with an empty root.
    pub fn memory() -> Self {
        Self::new(None)
    }

    /// The read-only base image, when present.
    pub fn base(&self) -> Option<&'static dyn FileSystem> {
        self.base
    }

    fn base_meta(&self, path: &str) -> Option<Meta> {
        let base = self.base?;
        let node = base.resolve(path)?;
        if base.is_dir(node) {
            Some(Meta {
                kind: Kind::Dir,
                size: 0,
            })
        } else if base.is_file(node) {
            Some(Meta {
                kind: Kind::File,
                size: base.size(node),
            })
        } else {
            None
        }
    }

    fn is_tombstone(&self, path: &str) -> bool {
        matches!(self.nodes.get(path), Some(Node::Tombstone))
    }

    /// True when any path component, including `path`, is deleted.
    fn hidden(&self, path: &str) -> bool {
        let mut prefix = String::new();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            prefix.push('/');
            prefix.push_str(part);
            if self.is_tombstone(&prefix) {
                return true;
            }
        }
        false
    }

    /// True when an overlay entry at a proper ancestor hides the base image.
    fn overlay_ancestor(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let mut prefix = String::new();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            prefix.push('/');
            prefix.push_str(part);
            if self.nodes.contains_key(&prefix) {
                return true;
            }
        }
        false
    }

    fn effective_meta(&self, path: &str) -> Result<Meta, FsError> {
        if self.hidden(path) {
            return Err(FsError::NotFound);
        }
        if path == "/" {
            return Ok(Meta {
                kind: Kind::Dir,
                size: 0,
            });
        }
        if let Some(node) = self.nodes.get(path) {
            return match node {
                Node::File(data) => Ok(Meta {
                    kind: Kind::File,
                    size: data.len(),
                }),
                Node::Dir => Ok(Meta {
                    kind: Kind::Dir,
                    size: 0,
                }),
                Node::Tombstone => Err(FsError::NotFound),
            };
        }
        if self.overlay_ancestor(path) {
            return Err(FsError::NotFound);
        }
        self.base_meta(path).ok_or(FsError::NotFound)
    }

    fn remove_tree(&mut self, path: &str) -> Result<(), FsError> {
        let meta = self.effective_meta(path)?;
        match meta.kind {
            Kind::File => self.remove_file(path),
            Kind::Dir => {
                if path == "/" {
                    return Err(FsError::InvalidPath);
                }
                let entries = self.read_dir(path)?;
                for entry in entries {
                    let child = join(path, &entry.name);
                    self.remove_tree(&child)?;
                }
                self.remove_dir(path)
            }
        }
    }

    fn copy_tree(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        let meta = self.effective_meta(from)?;
        match meta.kind {
            Kind::File => {
                let data = self.read_file(from)?;
                self.write_file(to, &data, false)
            }
            Kind::Dir => {
                self.create_dir(to)?;
                let entries = self.read_dir(from)?;
                for entry in entries {
                    let source = join(from, &entry.name);
                    let target = join(to, &entry.name);
                    self.copy_tree(&source, &target)?;
                }
                Ok(())
            }
        }
    }
}

impl Vfs for OverlayFs {
    fn cwd(&self) -> &str {
        &self.cwd
    }

    fn chdir(&mut self, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        let meta = self.effective_meta(&path)?;
        if meta.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        self.cwd = path;
        Ok(())
    }

    fn meta(&self, path: &str) -> Result<Meta, FsError> {
        let path = normalize(&self.cwd, path);
        self.effective_meta(&path)
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, FsError> {
        let path = normalize(&self.cwd, path);
        let meta = self.effective_meta(&path)?;
        if meta.kind != Kind::File {
            return Err(FsError::IsDir);
        }

        if let Some(Node::File(data)) = self.nodes.get(&path) {
            return Ok(data.clone());
        }

        let base = self.base.ok_or(FsError::NotFound)?;
        let node = base.resolve(&path).ok_or(FsError::NotFound)?;
        let size = base.size(node);
        let mut data = Vec::with_capacity(size);
        let mut chunk = [0u8; 4096];
        let mut offset = 0usize;
        loop {
            let read = base.read(node, offset, &mut chunk);
            if read == 0 {
                break;
            }
            data.extend_from_slice(&chunk[..read]);
            offset += read;
        }
        Ok(data)
    }

    fn write_file(&mut self, path: &str, data: &[u8], append: bool) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        if path == "/" {
            return Err(FsError::IsDir);
        }
        let parent = parent(&path);
        if self.effective_meta(parent)?.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        if let Ok(meta) = self.effective_meta(&path) {
            if meta.kind == Kind::Dir {
                return Err(FsError::IsDir);
            }
        }

        let contents = if append {
            match self.read_file(&path) {
                Ok(mut existing) => {
                    existing.extend_from_slice(data);
                    existing
                }
                Err(FsError::NotFound) => data.to_vec(),
                Err(error) => return Err(error),
            }
        } else {
            data.to_vec()
        };
        self.nodes.insert(path, Node::File(contents));
        Ok(())
    }

    fn create_dir(&mut self, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        if path == "/" {
            return Err(FsError::Exists);
        }
        let parent = parent(&path);
        if self.effective_meta(parent)?.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        if self.effective_meta(&path).is_ok() {
            return Err(FsError::Exists);
        }
        self.nodes.insert(path, Node::Dir);
        Ok(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        let meta = self.effective_meta(&path)?;
        if meta.kind == Kind::Dir {
            return Err(FsError::IsDir);
        }
        if self.base_meta(&path).is_some() {
            self.nodes.insert(path, Node::Tombstone);
        } else {
            self.nodes.remove(&path);
        }
        Ok(())
    }

    fn remove_dir(&mut self, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        if path == "/" {
            return Err(FsError::InvalidPath);
        }
        let meta = self.effective_meta(&path)?;
        if meta.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        if !self.read_dir(&path)?.is_empty() {
            return Err(FsError::NotEmpty);
        }

        let prefix = join(&path, "");
        let nested: Vec<String> = self
            .nodes
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        for key in nested {
            self.nodes.remove(&key);
        }

        if self.base_meta(&path).is_some() {
            self.nodes.insert(path, Node::Tombstone);
        } else {
            self.nodes.remove(&path);
        }
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        let from = normalize(&self.cwd, from);
        let to = normalize(&self.cwd, to);
        if from == "/" || to == "/" || from == to {
            return Err(FsError::InvalidPath);
        }

        let inside = format!("{from}/");
        if to.starts_with(&inside) {
            return Err(FsError::InvalidPath);
        }

        self.effective_meta(&from)?;
        let parent = parent(&to);
        if self.effective_meta(parent)?.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        if self.effective_meta(&to).is_ok() {
            return Err(FsError::Exists);
        }

        self.copy_tree(&from, &to)?;
        self.remove_tree(&from)?;
        Ok(())
    }

    fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let path = normalize(&self.cwd, path);
        let meta = self.effective_meta(&path)?;
        if meta.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }

        let mut entries: BTreeMap<String, DirEntry> = BTreeMap::new();
        let shadowed =
            self.overlay_ancestor(&path) || matches!(self.nodes.get(&path), Some(Node::Dir));

        if !shadowed {
            if let Some(base) = self.base {
                if let Some(dir) = base.resolve(&path) {
                    for index in 0..base.child_count(dir) {
                        let Some(child) = base.child_at(dir, index) else {
                            continue;
                        };
                        let Some(name) = base.name(child) else {
                            continue;
                        };
                        let child_path = join(&path, name);
                        if self.nodes.contains_key(&child_path) || self.hidden(&child_path) {
                            continue;
                        }
                        let kind = if base.is_dir(child) {
                            Kind::Dir
                        } else {
                            Kind::File
                        };
                        let size = if kind == Kind::File {
                            base.size(child)
                        } else {
                            0
                        };
                        entries.insert(
                            name.to_string(),
                            DirEntry {
                                name: name.to_string(),
                                kind,
                                size,
                            },
                        );
                    }
                }
            }
        }

        for (key, node) in &self.nodes {
            if matches!(node, Node::Tombstone) || parent(key) != path {
                continue;
            }
            if self.hidden(key) {
                continue;
            }
            let (kind, size) = match node {
                Node::File(data) => (Kind::File, data.len()),
                Node::Dir => (Kind::Dir, 0),
                Node::Tombstone => continue,
            };
            let name = base_name(key).to_string();
            entries.insert(name.clone(), DirEntry { name, kind, size });
        }

        Ok(entries.into_iter().map(|(_, entry)| entry).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static NODES: &[feos_fs::StaticNode] = &[
        feos_fs::StaticNode::dir("/", None, &[1, 2]),
        feos_fs::StaticNode::file("readme.txt", Some(0), b"hello\n"),
        feos_fs::StaticNode::dir("docs", Some(0), &[3]),
        feos_fs::StaticNode::file("guide.md", Some(2), b"# Guide\n"),
    ];
    static BASE: feos_fs::StaticFs = feos_fs::StaticFs::new(NODES);

    #[test]
    fn normalises_paths() {
        assert_eq!(normalize("/", ""), "/");
        assert_eq!(normalize("/a/b", "../c"), "/a/c");
        assert_eq!(normalize("/", "../../x"), "/x");
        assert_eq!(normalize("/a", "b/./c/"), "/a/b/c");
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/"), "/");
        assert_eq!(base_name("/a/b"), "b");
        assert_eq!(join("/", "x"), "/x");
    }

    #[test]
    fn reads_through_to_the_base_image() {
        let mut fs = OverlayFs::new(Some(&BASE));
        assert_eq!(fs.cwd(), "/");
        assert!(fs.is_dir("/docs"));
        let data = fs.read_file("/docs/guide.md").unwrap();
        assert_eq!(data, b"# Guide\n");
    }

    #[test]
    fn writes_shadow_the_base_and_deletes_create_tombstones() {
        let mut fs = OverlayFs::new(Some(&BASE));

        fs.write_file("/readme.txt", b"new", false).unwrap();
        assert_eq!(fs.read_file("/readme.txt").unwrap(), b"new");
        fs.write_file("/readme.txt", b"!", true).unwrap();
        assert_eq!(fs.read_file("/readme.txt").unwrap(), b"new!");

        fs.create_dir("/tmp").unwrap();
        fs.write_file("/tmp/a.txt", b"a", false).unwrap();
        assert_eq!(fs.read_dir("/").unwrap().len(), 3);

        fs.remove_file("/docs/guide.md").unwrap();
        assert_eq!(fs.read_dir("/docs").unwrap().len(), 0);
        assert!(!fs.exists("/docs/guide.md"));
        fs.remove_dir("/docs").unwrap();
        assert!(!fs.exists("/docs"));
    }

    #[test]
    fn renames_files_and_directories() {
        let mut fs = OverlayFs::memory();
        fs.create_dir("/a").unwrap();
        fs.write_file("/a/file", b"payload", false).unwrap();
        fs.rename("/a", "/b").unwrap();
        assert!(!fs.exists("/a"));
        assert_eq!(fs.read_file("/b/file").unwrap(), b"payload");
        fs.rename("/b/file", "/b/renamed").unwrap();
        assert_eq!(fs.read_file("/b/renamed").unwrap(), b"payload");
    }

    #[test]
    fn refuses_to_remove_non_empty_directories() {
        let mut fs = OverlayFs::memory();
        fs.create_dir("/a").unwrap();
        fs.write_file("/a/file", b"x", false).unwrap();
        assert_eq!(fs.remove_dir("/a"), Err(FsError::NotEmpty));
        fs.remove_file("/a/file").unwrap();
        fs.remove_dir("/a").unwrap();
    }
}
