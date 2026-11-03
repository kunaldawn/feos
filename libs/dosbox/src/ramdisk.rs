//! A small writable RAM drive for `C:`.
//!
//! The FEOS filesystem exposed to apps is read-only, so FEOS-DOS owns a RAM
//! drive. At start it copies the board's files into `C:\FEOS`, then DOS
//! programs and the prompt can create, change and delete files freely without
//! touching the host image.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use feos_fs::{EntryKind, FileSystem, NodeId};

/// One file or directory on the RAM drive.
#[derive(Clone)]
pub struct Node {
    pub name: String,
    pub is_dir: bool,
    pub data: Vec<u8>,
    pub children: Vec<usize>,
    pub parent: Option<usize>,
    /// DOS attribute byte (`0x10` directory, `0x20` archive).
    pub attr: u8,
    pub date: u16,
    pub time: u16,
}

impl Node {
    fn new(name: String, is_dir: bool, parent: Option<usize>) -> Self {
        Self {
            name,
            is_dir,
            data: Vec::new(),
            children: Vec::new(),
            parent,
            attr: if is_dir { 0x10 } else { 0x20 },
            date: 0x5C21,
            time: 0,
        }
    }
}

/// The writable `C:` drive.
pub struct RamDisk {
    pub nodes: Vec<Node>,
    pub root: usize,
}

impl Default for RamDisk {
    fn default() -> Self {
        Self::new()
    }
}

impl RamDisk {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node::new("\\".to_string(), true, None)],
            root: 0,
        }
    }

    pub fn node(&self, id: usize) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn node_mut(&mut self, id: usize) -> Option<&mut Node> {
        self.nodes.get_mut(id)
    }

    pub fn is_dir(&self, id: usize) -> bool {
        self.nodes.get(id).is_some_and(|node| node.is_dir)
    }

    pub fn is_file(&self, id: usize) -> bool {
        self.nodes.get(id).is_some_and(|node| !node.is_dir)
    }

    pub fn size(&self, id: usize) -> usize {
        self.nodes.get(id).map_or(0, |node| node.data.len())
    }

    pub fn children(&self, id: usize) -> &[usize] {
        self.nodes
            .get(id)
            .map_or(&[], |node| node.children.as_slice())
    }

    /// Looks up a direct child, case-insensitively, by either its long name or
    /// its DOS 8.3 name.
    pub fn lookup(&self, dir: usize, name: &str) -> Option<usize> {
        let wanted = normalize(name);
        let wanted_dos = dos_name(name);
        let node = self.nodes.get(dir)?;
        for &child in &node.children {
            let child_node = &self.nodes[child];
            if normalize(&child_node.name) == wanted || dos_name(&child_node.name) == wanted_dos {
                return Some(child);
            }
        }
        None
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
                current = self.nodes.get(current)?.parent?;
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
        while let Some(node) = self.nodes.get(current) {
            if current == self.root {
                break;
            }
            parts.push(&node.name);
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

    pub fn create_dir(&mut self, parent: usize, name: &str) -> Option<usize> {
        if name.is_empty() || self.lookup(parent, name).is_some() {
            return None;
        }
        let id = self.nodes.len();
        self.nodes
            .push(Node::new(name.to_string(), true, Some(parent)));
        self.nodes[parent].children.push(id);
        Some(id)
    }

    pub fn create_file(&mut self, parent: usize, name: &str, data: Vec<u8>) -> Option<usize> {
        if name.is_empty() {
            return None;
        }
        if let Some(existing) = self.lookup(parent, name) {
            if self.nodes[existing].is_dir {
                return None;
            }
            self.nodes[existing].data = data;
            return Some(existing);
        }
        let id = self.nodes.len();
        self.nodes
            .push(Node::new(name.to_string(), false, Some(parent)));
        self.nodes[id].data = data;
        self.nodes[parent].children.push(id);
        Some(id)
    }

    pub fn remove(&mut self, id: usize) -> bool {
        if id == self.root || id >= self.nodes.len() {
            return false;
        }
        let Some(node) = self.nodes.get(id) else {
            return false;
        };
        if node.is_dir && !node.children.is_empty() {
            return false;
        }
        let parent = node.parent;
        if let Some(parent) = parent {
            if let Some(children) = self.nodes.get_mut(parent).map(|node| &mut node.children) {
                children.retain(|&child| child != id);
            }
        }
        // Keep the node table stable; mark it as an orphan by clearing the
        // name. This avoids invalidating ids held by open handles.
        self.nodes[id].name.clear();
        self.nodes[id].parent = None;
        true
    }

    pub fn rename(&mut self, id: usize, new_parent: usize, new_name: &str) -> bool {
        if id == self.root || id >= self.nodes.len() || new_name.is_empty() {
            return false;
        }
        if self.lookup(new_parent, new_name).is_some() {
            return false;
        }
        let old_parent = self.nodes[id].parent;
        if let Some(parent) = old_parent {
            if let Some(children) = self.nodes.get_mut(parent).map(|node| &mut node.children) {
                children.retain(|&child| child != id);
            }
        }
        self.nodes[id].name = new_name.to_string();
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

    pub fn write(&mut self, id: usize, offset: usize, data: &[u8]) -> usize {
        let Some(node) = self.nodes.get_mut(id) else {
            return 0;
        };
        if node.is_dir {
            return 0;
        }
        if offset > node.data.len() {
            node.data.resize(offset, 0);
        }
        let end = offset.saturating_add(data.len());
        if end > node.data.len() {
            node.data.resize(end, 0);
        }
        node.data[offset..end].copy_from_slice(data);
        data.len()
    }

    pub fn truncate(&mut self, id: usize, size: usize) -> bool {
        let Some(node) = self.nodes.get_mut(id) else {
            return false;
        };
        if node.is_dir {
            return false;
        }
        node.data.resize(size, 0);
        true
    }

    /// Finds the children whose DOS name matches a `*`/`?` pattern.
    pub fn find(&self, dir: usize, pattern: &str) -> Vec<usize> {
        let pattern = dos_name(pattern);
        let mut out = Vec::new();
        for &child in self.children(dir) {
            let name = dos_name(&self.nodes[child].name);
            if wildcard_match(&name, &pattern) {
                out.push(child);
            }
        }
        out
    }

    pub fn total_bytes(&self) -> usize {
        self.nodes.iter().map(|node| node.data.len()).sum()
    }

    /// Copies a read-only FEOS filesystem into `dest`, recursively.
    pub fn import_fs(
        &mut self,
        dest: usize,
        fs: &dyn FileSystem,
        source: NodeId,
        depth: usize,
    ) -> usize {
        if depth > 8 {
            return 0;
        }
        let mut copied = 0;
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
                        copied += 1;
                        copied += self.import_fs(dir, fs, child, depth + 1);
                    }
                }
                Some(EntryKind::File) => {
                    let size = fs.size(child);
                    if size > 4 * 1024 * 1024 {
                        continue;
                    }
                    let mut data = vec![0u8; size];
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
                        copied += 1;
                    }
                }
                None => {}
            }
        }
        copied
    }
}

/// Uppercases a name for comparison.
pub fn normalize(name: &str) -> String {
    name.to_ascii_uppercase()
}

/// Converts a long name to the 8.3 form DOS displays.
pub fn dos_name(name: &str) -> String {
    let upper = name.to_ascii_uppercase();
    if upper == "\\" {
        return upper;
    }
    let (base, extension) = match upper.rsplit_once('.') {
        Some((base, extension)) if !base.is_empty() => (base, extension),
        _ => (upper.as_str(), ""),
    };
    let mut out = String::new();
    for ch in base.chars().take(8) {
        out.push(ch);
    }
    if !extension.is_empty() {
        out.push('.');
        for ch in extension.chars().take(3) {
            out.push(ch);
        }
    }
    out
}

/// Removes a leading `C:` (or any drive letter) from a path.
pub fn strip_drive(path: &str) -> &str {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        &path[2..]
    } else {
        path
    }
}

/// Matches a DOS name against a `*`/`?` pattern, case-insensitively.
pub fn wildcard_match(name: &str, pattern: &str) -> bool {
    let name = name.to_ascii_uppercase();
    let pattern = pattern.to_ascii_uppercase();
    let name = name.as_bytes();
    let pattern = pattern.as_bytes();
    let (mut n, mut p) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == name[n]) {
            n += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
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
}
