//! A small writable RAM drive for `C:`.
//!
//! The FEOS filesystem exposed to apps is read-only, so FEOS-DOS owns a RAM
//! drive. `MOUNT` copies directories and ZIPs into it on demand, and DOS
//! programs and the prompt can create, change and delete files freely without
//! touching the host image.
//!
//! The drive has a quota, and every allocation is fallible, so neither a guest
//! nor a large mount can exhaust the kernel's heap through it.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_fs::{EntryKind, FileSystem, NodeId};

/// The most file data the drive holds. It's below the kernel heap's size so the
/// machine itself still has room.
pub const DRIVE_QUOTA: usize = 40 * 1024 * 1024;
/// The most files and directories the drive holds.
pub const MAX_NODES: usize = 16 * 1024;

/// One file or directory on the RAM drive.
#[derive(Clone)]
pub struct Node {
    pub name: String,
    /// The unique 8.3 name DOS programs see.
    pub short: String,
    pub is_dir: bool,
    pub data: Vec<u8>,
    pub children: Vec<usize>,
    pub parent: Option<usize>,
    /// DOS attribute byte (`0x10` directory, `0x20` archive).
    pub attr: u8,
    pub date: u16,
    pub time: u16,
    /// How many system file table entries refer to this node.
    pub open_count: u32,
    /// Deleted, and kept only because it's still open.
    pub removed: bool,
}

impl Node {
    fn new(name: String, short: String, is_dir: bool, parent: Option<usize>) -> Self {
        Self {
            name,
            short,
            is_dir,
            data: Vec::new(),
            children: Vec::new(),
            parent,
            attr: if is_dir { 0x10 } else { 0x20 },
            date: 0x5C21,
            time: 0,
            open_count: 0,
            removed: false,
        }
    }

    fn free(&self) -> bool {
        self.removed && self.open_count == 0
    }
}

/// The writable `C:` drive.
pub struct RamDisk {
    pub nodes: Vec<Node>,
    pub root: usize,
    used: usize,
    pub quota: usize,
}

impl Default for RamDisk {
    fn default() -> Self {
        Self::new()
    }
}

/// What an import copied and what it had to leave behind.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ImportStats {
    pub copied: usize,
    pub skipped: usize,
}

impl RamDisk {
    pub fn new() -> Self {
        Self {
            nodes: alloc::vec![Node::new("\\".to_string(), "\\".to_string(), true, None)],
            root: 0,
            used: 0,
            quota: DRIVE_QUOTA,
        }
    }

    pub fn node(&self, id: usize) -> Option<&Node> {
        self.nodes.get(id).filter(|node| !node.removed)
    }

    pub fn node_mut(&mut self, id: usize) -> Option<&mut Node> {
        self.nodes.get_mut(id).filter(|node| !node.removed)
    }

    pub fn is_dir(&self, id: usize) -> bool {
        self.node(id).is_some_and(|node| node.is_dir)
    }

    pub fn is_file(&self, id: usize) -> bool {
        self.node(id).is_some_and(|node| !node.is_dir)
    }

    pub fn size(&self, id: usize) -> usize {
        self.nodes.get(id).map_or(0, |node| node.data.len())
    }

    pub fn children(&self, id: usize) -> &[usize] {
        self.node(id).map_or(&[], |node| node.children.as_slice())
    }

    /// Looks up a direct child, case-insensitively, by either its long name or
    /// its DOS 8.3 name.
    pub fn lookup(&self, dir: usize, name: &str) -> Option<usize> {
        let wanted = normalize(name);
        let wanted_dos = dos_name(name);
        let node = self.node(dir)?;
        node.children.iter().copied().find(|&child| {
            let child_node = &self.nodes[child];
            child_node.short == wanted_dos || normalize(&child_node.name) == wanted
        })
    }

    /// Resolves a DOS path relative to `from`.
    pub fn resolve(&self, from: usize, path: &str) -> Option<usize> {
        let path = strip_drive(path);
        if path.is_empty() {
            return Some(from);
        }
        let mut current = if path.starts_with('\\') || path.starts_with('/') {
            self.root
        } else {
            from
        };
        for component in path.split(['\\', '/']) {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                current = self.node(current)?.parent?;
                continue;
            }
            current = self.lookup(current, component)?;
        }
        Some(current)
    }

    /// Returns the DOS path of a node (`\`, `\DIR`, `\DIR\FILE.TXT`).
    pub fn path(&self, id: usize) -> String {
        if id == self.root {
            return "\\".to_string();
        }
        let mut parts: Vec<&str> = Vec::new();
        let mut current = id;
        // The tree has no cycles, but a walk must never run away if it did.
        for _ in 0..=self.nodes.len() {
            let Some(node) = self.nodes.get(current) else {
                break;
            };
            if current == self.root {
                break;
            }
            parts.push(&node.short);
            current = match node.parent {
                Some(parent) => parent,
                None => break,
            };
        }
        let mut out = String::new();
        for part in parts.iter().rev() {
            out.push('\\');
            out.push_str(part);
        }
        if out.is_empty() {
            out.push('\\');
        }
        out
    }

    /// The 8.3 name a new entry gets: unique in `parent`, with a `~n` tail when
    /// the long name doesn't fit or collides.
    fn make_short(&self, parent: usize, name: &str) -> Option<String> {
        let exact = dos_name(name);
        let fits = normalize(name) == exact;
        let taken = |candidate: &str| {
            self.children(parent)
                .iter()
                .any(|&child| self.nodes[child].short == candidate)
        };
        if fits && !taken(&exact) {
            return Some(exact);
        }
        let (base, extension) = match exact.rsplit_once('.') {
            Some((base, extension)) => (base, extension),
            None => (exact.as_str(), ""),
        };
        for tail in 1..10_000u32 {
            let suffix = format!("~{tail}");
            let keep = 8usize.saturating_sub(suffix.len());
            let stem: String = base.chars().take(keep).collect();
            let candidate = if extension.is_empty() {
                format!("{stem}{suffix}")
            } else {
                format!("{stem}{suffix}.{extension}")
            };
            if !taken(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Adds a node, reusing a freed slot so node ids stay bounded.
    fn add_node(&mut self, node: Node) -> Option<usize> {
        if let Some(id) = self.nodes.iter().position(Node::free) {
            self.nodes[id] = node;
            return Some(id);
        }
        if self.nodes.len() >= MAX_NODES || self.nodes.try_reserve(1).is_err() {
            return None;
        }
        self.nodes.push(node);
        Some(self.nodes.len() - 1)
    }

    fn valid_parent(&self, parent: usize) -> bool {
        self.is_dir(parent)
    }

    pub fn create_dir(&mut self, parent: usize, name: &str) -> Option<usize> {
        if !valid_name(name) || !self.valid_parent(parent) || self.lookup(parent, name).is_some() {
            return None;
        }
        let short = self.make_short(parent, name)?;
        let id = self.add_node(Node::new(name.to_string(), short, true, Some(parent)))?;
        self.nodes[parent].children.push(id);
        Some(id)
    }

    /// Creates a file, or replaces the data of the one with that name.
    pub fn create_file(&mut self, parent: usize, name: &str, data: Vec<u8>) -> Option<usize> {
        if !valid_name(name) || !self.valid_parent(parent) {
            return None;
        }
        if let Some(existing) = self.lookup(parent, name) {
            if self.nodes[existing].is_dir {
                return None;
            }
            let old = self.nodes[existing].data.len();
            if self.used - old + data.len() > self.quota {
                return None;
            }
            self.used = self.used - old + data.len();
            self.nodes[existing].data = data;
            return Some(existing);
        }
        if self.used + data.len() > self.quota {
            return None;
        }
        let short = self.make_short(parent, name)?;
        let mut node = Node::new(name.to_string(), short, false, Some(parent));
        self.used += data.len();
        node.data = data;
        let id = self.add_node(node)?;
        self.nodes[parent].children.push(id);
        Some(id)
    }

    /// Unlinks a file or an empty directory. An open file's data stays until
    /// its last handle closes.
    pub fn remove(&mut self, id: usize) -> bool {
        if id == self.root {
            return false;
        }
        let Some(node) = self.node(id) else {
            return false;
        };
        if node.is_dir && !node.children.is_empty() {
            return false;
        }
        if let Some(parent) = node.parent {
            self.nodes[parent].children.retain(|&child| child != id);
        }
        let node = &mut self.nodes[id];
        node.parent = None;
        node.removed = true;
        if node.open_count == 0 {
            let freed = node.data.len();
            node.data = Vec::new();
            self.used -= freed;
        }
        true
    }

    /// Removes a directory and everything under it, as long as no file in it is
    /// open. Returns false, with nothing removed, otherwise.
    pub fn remove_tree(&mut self, id: usize) -> bool {
        if !self.tree_closed(id, 0) {
            return false;
        }
        self.remove_tree_inner(id, 0);
        true
    }

    fn tree_closed(&self, id: usize, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        let Some(node) = self.node(id) else {
            return false;
        };
        node.open_count == 0
            && node
                .children
                .iter()
                .all(|&child| self.tree_closed(child, depth + 1))
    }

    fn remove_tree_inner(&mut self, id: usize, depth: usize) {
        if depth > 64 {
            return;
        }
        let children = self.children(id).to_vec();
        for child in children {
            self.remove_tree_inner(child, depth + 1);
        }
        self.remove(id);
    }

    /// Records a new open file table reference.
    pub fn open(&mut self, id: usize) {
        if let Some(node) = self.nodes.get_mut(id) {
            node.open_count = node.open_count.saturating_add(1);
        }
    }

    /// Drops an open file table reference, freeing a deleted file's data.
    pub fn close(&mut self, id: usize) {
        let Some(node) = self.nodes.get_mut(id) else {
            return;
        };
        node.open_count = node.open_count.saturating_sub(1);
        if node.free() {
            let freed = node.data.len();
            node.data = Vec::new();
            self.used -= freed;
        }
    }

    /// Whether `id` is `ancestor` or lies somewhere under it.
    pub fn is_within(&self, id: usize, ancestor: usize) -> bool {
        let mut current = Some(id);
        for _ in 0..=self.nodes.len() {
            let Some(node) = current else {
                return false;
            };
            if node == ancestor {
                return true;
            }
            current = self.nodes.get(node).and_then(|entry| entry.parent);
        }
        false
    }

    pub fn rename(&mut self, id: usize, new_parent: usize, new_name: &str) -> bool {
        if id == self.root || !valid_name(new_name) || self.node(id).is_none() {
            return false;
        }
        if !self.valid_parent(new_parent) || self.is_within(new_parent, id) {
            return false;
        }
        let old_parent = self.nodes[id].parent;
        // Renaming to its own name is allowed, even when only the case changes.
        if let Some(existing) = self.lookup(new_parent, new_name) {
            if existing != id {
                return false;
            }
        }
        if let Some(parent) = old_parent {
            self.nodes[parent].children.retain(|&child| child != id);
        }
        let Some(short) = self.make_short(new_parent, new_name) else {
            if let Some(parent) = old_parent {
                self.nodes[parent].children.push(id);
            }
            return false;
        };
        self.nodes[id].name = new_name.to_string();
        self.nodes[id].short = short;
        self.nodes[id].parent = Some(new_parent);
        self.nodes[new_parent].children.push(id);
        true
    }

    pub fn read(&self, id: usize, offset: usize, out: &mut [u8]) -> usize {
        let Some(node) = self.nodes.get(id) else {
            return 0;
        };
        if offset >= node.data.len() {
            return 0;
        }
        let count = out.len().min(node.data.len() - offset);
        out[..count].copy_from_slice(&node.data[offset..offset + count]);
        count
    }

    /// Grows a file's data to `size`, within the quota and the heap.
    fn grow(&mut self, id: usize, size: usize) -> bool {
        let Some(node) = self.nodes.get_mut(id) else {
            return false;
        };
        let old = node.data.len();
        if size <= old {
            return true;
        }
        let more = size - old;
        if self.used.saturating_add(more) > self.quota || node.data.try_reserve(more).is_err() {
            return false;
        }
        node.data.resize(size, 0);
        self.used += more;
        true
    }

    /// Writes into a file, extending it. Returns how many bytes fit, which is
    /// short of the request when the drive is full.
    pub fn write(&mut self, id: usize, offset: usize, data: &[u8]) -> usize {
        if self.nodes.get(id).is_none_or(|node| node.is_dir) || data.is_empty() {
            return 0;
        }
        let Some(wanted_end) = offset.checked_add(data.len()) else {
            return 0;
        };
        let room = self.quota.saturating_sub(self.used) + self.size(id);
        let end = wanted_end.min(room);
        if end <= offset || !self.grow(id, end) {
            return 0;
        }
        let count = end - offset;
        self.nodes[id].data[offset..end].copy_from_slice(&data[..count]);
        count
    }

    /// Sets a file's size. Returns false when it can't grow that far.
    pub fn truncate(&mut self, id: usize, size: usize) -> bool {
        let Some(node) = self.nodes.get(id) else {
            return false;
        };
        if node.is_dir {
            return false;
        }
        let old = node.data.len();
        if size > old {
            return self.grow(id, size);
        }
        self.nodes[id].data.truncate(size);
        self.used -= old - size;
        true
    }

    /// Finds the children whose DOS name matches a `*`/`?` pattern.
    pub fn find(&self, dir: usize, pattern: &str) -> Vec<usize> {
        let pattern = fcb_pattern(pattern);
        self.children(dir)
            .iter()
            .copied()
            .filter(|&child| fcb_match(&fcb_name(&self.nodes[child].short), &pattern))
            .collect()
    }

    pub fn total_bytes(&self) -> usize {
        self.used
    }

    /// The bytes left under the quota.
    pub fn free_bytes(&self) -> usize {
        self.quota.saturating_sub(self.used)
    }

    /// Copies a read-only FEOS filesystem into `dest`, recursively. Files that
    /// don't fit are counted, not silently dropped.
    pub fn import_fs(
        &mut self,
        dest: usize,
        fs: &dyn FileSystem,
        source: NodeId,
        depth: usize,
    ) -> ImportStats {
        let mut stats = ImportStats::default();
        if depth > 8 {
            return stats;
        }
        let count = fs.child_count(source);
        for index in 0..count {
            let Some(child) = fs.child_at(source, index) else {
                continue;
            };
            let Some(name) = fs.name(child) else {
                continue;
            };
            match fs.kind(child) {
                Some(EntryKind::Directory) => {
                    if let Some(dir) = self.create_dir(dest, name) {
                        stats.copied += 1;
                        let inner = self.import_fs(dir, fs, child, depth + 1);
                        stats.copied += inner.copied;
                        stats.skipped += inner.skipped;
                    } else {
                        stats.skipped += 1;
                    }
                }
                Some(EntryKind::File) => {
                    let size = fs.size(child);
                    let mut data = Vec::new();
                    if size > self.free_bytes() || data.try_reserve_exact(size).is_err() {
                        stats.skipped += 1;
                        continue;
                    }
                    data.resize(size, 0);
                    let mut offset = 0;
                    while offset < size {
                        let read = fs.read(child, offset, &mut data[offset..]);
                        if read == 0 {
                            break;
                        }
                        offset += read;
                    }
                    data.truncate(offset);
                    if self.create_file(dest, name, data).is_some() {
                        stats.copied += 1;
                    } else {
                        stats.skipped += 1;
                    }
                }
                None => {}
            }
        }
        stats
    }
}

/// Uppercases a name for comparison.
pub fn normalize(name: &str) -> String {
    name.to_ascii_uppercase()
}

/// Whether a name can be created. Wildcards and path characters can't be part
/// of one.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.chars().all(|ch| {
            !matches!(ch, '*' | '?' | '/' | '\\' | ':' | '<' | '>' | '|' | '"') && ch >= ' '
        })
}

/// A character as it appears in an 8.3 name.
fn short_char(ch: char) -> Option<char> {
    match ch {
        ' ' => None,
        '+' | ',' | ';' | '=' | '[' | ']' => Some('_'),
        ch if ch.is_ascii() => Some(ch.to_ascii_uppercase()),
        _ => Some('_'),
    }
}

/// Converts a long name to the 8.3 form DOS displays.
pub fn dos_name(name: &str) -> String {
    if name == "\\" {
        return name.to_string();
    }
    let (base, extension) = match name.rsplit_once('.') {
        Some((base, extension)) if !base.is_empty() => (base, extension),
        _ => (name, ""),
    };
    let mut out = String::new();
    for ch in base
        .chars()
        .filter_map(short_char)
        .filter(|ch| *ch != '.')
        .take(8)
    {
        out.push(ch);
    }
    let extension: String = extension.chars().filter_map(short_char).take(3).collect();
    if !extension.is_empty() {
        out.push('.');
        out.push_str(&extension);
    }
    out
}

/// Removes a leading `C:`. Other drive letters stay, so resolving them fails.
pub fn strip_drive(path: &str) -> &str {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].eq_ignore_ascii_case(&b'C') && bytes[1] == b':' {
        &path[2..]
    } else {
        path
    }
}

/// An 8.3 name in the 11-byte space-padded form.
pub fn fcb_name(short: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    let (base, extension) = match short.rsplit_once('.') {
        Some((base, extension)) => (base, extension),
        None => (short, ""),
    };
    for (slot, byte) in out[..8].iter_mut().zip(base.bytes()) {
        *slot = byte.to_ascii_uppercase();
    }
    for (slot, byte) in out[8..].iter_mut().zip(extension.bytes()) {
        *slot = byte.to_ascii_uppercase();
    }
    out
}

/// A search pattern in the 11-byte form, where `*` fills its field with `?`.
pub fn fcb_pattern(pattern: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    if pattern == "." {
        return *b"???????????";
    }
    let (base, extension) = match pattern.rsplit_once('.') {
        Some((base, extension)) => (base, extension),
        None => (pattern, ""),
    };
    let fill = |field: &mut [u8], text: &str| {
        for (at, byte) in text.bytes().enumerate().take(field.len()) {
            if byte == b'*' {
                for slot in field[at..].iter_mut() {
                    *slot = b'?';
                }
                return;
            }
            field[at] = byte.to_ascii_uppercase();
        }
    };
    let (name, ext) = out.split_at_mut(8);
    fill(name, base);
    fill(ext, extension);
    out
}

/// Matches an 11-byte name against an 11-byte pattern.
pub fn fcb_match(name: &[u8; 11], pattern: &[u8; 11]) -> bool {
    name.iter()
        .zip(pattern.iter())
        .all(|(name, pattern)| *pattern == b'?' || name == pattern)
}

/// Matches a DOS name against a `*`/`?` pattern, the way DOS does: `*` fills its
/// field, and `?` also matches the padding of a short name.
pub fn wildcard_match(name: &str, pattern: &str) -> bool {
    fcb_match(&fcb_name(&dos_name(name)), &fcb_pattern(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_is_case_insensitive_and_honours_dotdot() {
        let mut disk = RamDisk::new();
        let docs = disk.create_dir(0, "Documents").expect("dir");
        let file = disk
            .create_file(docs, "readme.txt", b"hi".to_vec())
            .expect("file");
        assert_eq!(disk.resolve(0, "c:\\DOCUMENTS\\README.TXT"), Some(file));
        assert_eq!(disk.resolve(docs, "..\\documents"), Some(docs));
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard_match("README.TXT", "*.TXT"));
        assert!(wildcard_match("A.COM", "?.*"));
        assert!(!wildcard_match("A.COM", "*.EXE"));
        assert!(wildcard_match("NOEXT", "*.*"));
        assert!(wildcard_match("A.B", "????????.???"));
    }

    #[test]
    fn writes_extend_files() {
        let mut disk = RamDisk::new();
        let file = disk.create_file(0, "A.TXT", Vec::new()).expect("file");
        assert_eq!(disk.write(file, 2, b"hi"), 2);
        assert_eq!(disk.size(file), 4);
        let mut out = [0u8; 4];
        assert_eq!(disk.read(file, 0, &mut out), 4);
        assert_eq!(&out, b"\0\0hi");
    }

    #[test]
    fn quota_stops_a_huge_seek_and_write() {
        let mut disk = RamDisk::new();
        let file = disk.create_file(0, "A.BIN", Vec::new()).expect("file");
        assert_eq!(disk.write(file, 0x7FFF_FFFF, b"x"), 0);
        assert_eq!(disk.total_bytes(), 0);
        disk.quota = 16;
        assert_eq!(disk.write(file, 0, &[1; 32]), 16);
        assert_eq!(disk.total_bytes(), 16);
        assert_eq!(disk.write(file, 16, &[1; 32]), 0);
    }

    #[test]
    fn deleted_files_keep_their_data_until_the_last_close() {
        let mut disk = RamDisk::new();
        let file = disk
            .create_file(0, "A.BIN", alloc::vec![7; 100])
            .expect("file");
        disk.open(file);
        assert!(disk.remove(file));
        assert_eq!(disk.resolve(0, "A.BIN"), None);
        assert_eq!(disk.total_bytes(), 100);
        let mut out = [0u8; 1];
        assert_eq!(disk.read(file, 0, &mut out), 1);
        disk.close(file);
        assert_eq!(disk.total_bytes(), 0);
        // The slot gets reused rather than growing the table forever.
        let count = disk.nodes.len();
        assert!(disk.create_file(0, "B.BIN", Vec::new()).is_some());
        assert_eq!(disk.nodes.len(), count);
    }

    #[test]
    fn directories_cannot_move_into_themselves() {
        let mut disk = RamDisk::new();
        let a = disk.create_dir(0, "A").expect("a");
        let b = disk.create_dir(a, "B").expect("b");
        assert!(!disk.rename(a, a, "B"));
        assert!(!disk.rename(a, b, "C"));
        assert_eq!(disk.path(b), "\\A\\B");
        assert!(disk.rename(b, 0, "C"));
        assert_eq!(disk.path(b), "\\C");
    }

    #[test]
    fn long_names_get_unique_short_names() {
        let mut disk = RamDisk::new();
        let first = disk
            .create_file(0, "longfilename1.txt", Vec::new())
            .expect("1");
        let second = disk
            .create_file(0, "longfilename2.txt", Vec::new())
            .expect("2");
        assert_ne!(first, second);
        assert_eq!(disk.nodes[first].short, "LONGFI~1.TXT");
        assert_eq!(disk.nodes[second].short, "LONGFI~2.TXT");
        assert_eq!(disk.resolve(0, "LONGFI~2.TXT"), Some(second));
        assert_eq!(disk.resolve(0, "longfilename1.txt"), Some(first));
    }

    #[test]
    fn wildcard_names_and_other_drives_are_rejected() {
        let mut disk = RamDisk::new();
        assert!(disk.create_file(0, "*.*", Vec::new()).is_none());
        assert!(disk.create_dir(0, "A?").is_none());
        disk.create_file(0, "A.TXT", Vec::new()).expect("file");
        assert!(disk.resolve(0, "C:\\A.TXT").is_some());
        assert!(disk.resolve(0, "D:\\A.TXT").is_none());
    }

    #[test]
    fn remove_tree_refuses_open_files() {
        let mut disk = RamDisk::new();
        let a = disk.create_dir(0, "A").expect("a");
        let file = disk.create_file(a, "F", alloc::vec![1; 8]).expect("f");
        disk.open(file);
        assert!(!disk.remove_tree(a));
        disk.close(file);
        assert!(disk.remove_tree(a));
        assert_eq!(disk.total_bytes(), 0);
        assert!(disk.resolve(0, "A").is_none());
    }
}
