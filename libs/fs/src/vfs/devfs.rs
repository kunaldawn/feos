//! The two synthesised filesystems the kernel mounts for programs: `/dev` and `/proc`
//! (section 3.3 of the ABI design, "`/dev` and `/proc/self`").
//!
//! Both are ordinary mounts of the shared tree, so every path call answers for them the
//! way it answers for anything else (`OPENAT`, `FSTATAT`, `GETDENTS`, links followed on
//! the way), and kernel apps see the same nodes. Neither can be changed: making,
//! removing, renaming or chmodding anything in them is [`FsError::NotPermitted`]
//! (`EPERM`), root included. `/dev/shm` is the exception, a RAM mount of its own on top.
//!
//! [`DevFs`] is a fixed table: the device nodes (their bytes made up by [`DeviceKind`])
//! and the links `stdin`, `stdout`, `stderr` and `fd` into `/proc/self/fd`. A link is
//! what the tree takes for one everywhere, a file with the system attribute holding
//! `XSym\n` and its target, made up here when it's read.
//!
//! [`ProcFs`] is whatever the *caller* is: `/proc/self` a link to its own pid, and
//! `/proc/<pid>` (only its own) holding `exe` and `fd/`. What a process is, is the
//! kernel's to say, behind [`ProcessView`]; a caller that isn't a program (a kernel app)
//! finds `/proc` empty. The view is asked with the tree's lock held, so it mustn't call
//! the tree back, and whatever lock it takes must never be held by anyone taking the
//! tree's.

use alloc::string::{String, ToString};

use feos_sync::SpinLock;

use super::{DeviceKind, MAX_LINK_TARGET, SYMLINK_MAGIC};
use crate::native::{PosixMeta, S_IFDIR, S_IFLNK};
use crate::{attr, EntryKind, FileSystem, FsError, NodeId, VolumeStats, WritableFileSystem};

/// A character device's type bits.
pub const S_IFCHR: u16 = 0o020000;

/// What `/proc` asks the kernel about the process making the call.
pub trait ProcessView: Sync {
    /// The caller's pid, or `None` if it isn't a program: then `/proc` has nothing in it.
    fn pid(&self) -> Option<u32>;
    /// The canonical path of the program it runs, as `CURRENT_EXE` gives it.
    fn exe(&self) -> Option<String>;
    /// How many descriptors it has open.
    fn descriptor_count(&self) -> usize;
    /// Its `index`th open descriptor, counting up from the lowest.
    fn nth_descriptor(&self, index: usize) -> Option<u32>;
    /// What `/proc/<pid>/fd/<fd>` leads to: the canonical path `FD_PATH` gives, or
    /// `pipe:[n]` for a pipe. `None` if `fd` isn't open.
    fn descriptor_target(&self, fd: u32) -> Option<String>;
}

/// The view the kernel installs, which the boot tree's `/proc` asks.
static INSTALLED: SpinLock<Option<&'static dyn ProcessView>> = SpinLock::new(None);

/// Makes `view` what the boot tree's `/proc` shows. Before any program runs.
pub fn set_process_view(view: &'static dyn ProcessView) {
    *INSTALLED.lock() = Some(view);
}

/// Whatever [`set_process_view`] installed, or nobody, as the view [`super::assemble`]
/// gives its `/proc`.
pub struct InstalledView;

impl InstalledView {
    fn view(&self) -> Option<&'static dyn ProcessView> {
        *INSTALLED.lock()
    }
}

impl ProcessView for InstalledView {
    fn pid(&self) -> Option<u32> {
        self.view()?.pid()
    }
    fn exe(&self) -> Option<String> {
        self.view()?.exe()
    }
    fn descriptor_count(&self) -> usize {
        self.view().map_or(0, |view| view.descriptor_count())
    }
    fn nth_descriptor(&self, index: usize) -> Option<u32> {
        self.view()?.nth_descriptor(index)
    }
    fn descriptor_target(&self, fd: u32) -> Option<String> {
        self.view()?.descriptor_target(fd)
    }
}

/// The view behind the boot tree's `/proc`.
pub static INSTALLED_VIEW: InstalledView = InstalledView;

/// The descriptor a canonical path names as `/proc/<pid>/fd/<fd>`, for the process
/// `pid`: what the kernel opens or looks at directly when the link leads nowhere a path
/// can go (a pipe), as Linux's "magic" links do.
pub fn descriptor_link(canonical: &str, pid: u32) -> Option<u32> {
    let rest = canonical.strip_prefix("/proc/")?;
    let (own, rest) = rest.split_once('/')?;
    let fd = rest.strip_prefix("fd/")?;
    (parse_number(own)? == pid).then_some(())?;
    parse_number(fd)
}

/// A decimal number as `/proc` spells one: digits only, no leading zero but `0` itself.
fn parse_number(text: &str) -> Option<u32> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// What one of `/dev`'s nodes is.
#[derive(Clone, Copy)]
enum DevNode {
    Root,
    Device(DeviceKind, u16),
    Link(&'static str),
}

/// `/dev`, sorted by name after its root.
const DEV: [(&str, DevNode); 12] = [
    ("/", DevNode::Root),
    ("console", DevNode::Device(DeviceKind::Console, 0o600)),
    ("fd", DevNode::Link("/proc/self/fd")),
    ("full", DevNode::Device(DeviceKind::Full, 0o666)),
    ("null", DevNode::Device(DeviceKind::Null, 0o666)),
    ("random", DevNode::Device(DeviceKind::Random, 0o666)),
    ("stderr", DevNode::Link("/proc/self/fd/2")),
    ("stdin", DevNode::Link("/proc/self/fd/0")),
    ("stdout", DevNode::Link("/proc/self/fd/1")),
    ("tty", DevNode::Device(DeviceKind::Tty, 0o666)),
    ("urandom", DevNode::Device(DeviceKind::Urandom, 0o666)),
    ("zero", DevNode::Device(DeviceKind::Zero, 0o666)),
];

/// Copies a link's stored form, `XSym\n` and `target`, from `offset` into `out`.
fn read_link_bytes(target: &str, offset: usize, out: &mut [u8]) -> usize {
    let magic = SYMLINK_MAGIC.len();
    let mut done = 0;
    let mut at = offset;
    while done < out.len() {
        let byte = if at < magic {
            SYMLINK_MAGIC[at]
        } else if let Some(&byte) = target.as_bytes().get(at - magic) {
            byte
        } else {
            break;
        };
        out[done] = byte;
        done += 1;
        at += 1;
    }
    done
}

/// `/dev`: see the module's description.
pub struct DevFs;

impl DevFs {
    fn node(node: NodeId) -> Option<(&'static str, DevNode)> {
        DEV.get(node.0 as usize).copied()
    }
}

impl FileSystem for DevFs {
    fn node_count(&self) -> usize {
        DEV.len()
    }

    fn kind(&self, node: NodeId) -> Option<EntryKind> {
        match Self::node(node)?.1 {
            DevNode::Root => Some(EntryKind::Directory),
            _ => Some(EntryKind::File),
        }
    }

    fn name(&self, node: NodeId) -> Option<&str> {
        Self::node(node).map(|(name, _)| name)
    }

    fn parent(&self, node: NodeId) -> Option<NodeId> {
        (node.0 != 0 && Self::node(node).is_some()).then_some(NodeId::ROOT)
    }

    fn size(&self, node: NodeId) -> usize {
        match Self::node(node) {
            Some((_, DevNode::Link(target))) => SYMLINK_MAGIC.len() + target.len(),
            _ => 0,
        }
    }

    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize {
        self.try_read(node, offset, out).unwrap_or(0)
    }

    fn try_read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        match Self::node(node).ok_or(FsError::NotFound)?.1 {
            DevNode::Root => Err(FsError::NotFile),
            DevNode::Device(kind, _) => Ok(kind.read(out)),
            DevNode::Link(target) => Ok(read_link_bytes(target, offset, out)),
        }
    }

    fn child_count(&self, node: NodeId) -> usize {
        if node == NodeId::ROOT { DEV.len() - 1 } else { 0 }
    }

    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId> {
        (node == NodeId::ROOT && index + 1 < DEV.len()).then(|| NodeId(index as u32 + 1))
    }

    fn attributes(&self, node: NodeId) -> u8 {
        match Self::node(node) {
            Some((_, DevNode::Link(_))) => attr::SYSTEM,
            _ => 0,
        }
    }

    fn stats(&self) -> VolumeStats {
        VolumeStats { block_size: 4096, total_blocks: 0, free_blocks: 0, read_only: false, kind: "devfs" }
    }
}

impl WritableFileSystem for DevFs {
    fn create_file(&mut self, _parent: NodeId, _name: &str) -> Result<NodeId, FsError> {
        Err(FsError::NotPermitted)
    }
    fn create_dir(&mut self, _parent: NodeId, _name: &str) -> Result<NodeId, FsError> {
        Err(FsError::NotPermitted)
    }
    fn remove(&mut self, _node: NodeId) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }
    fn rename(&mut self, _node: NodeId, _new_parent: NodeId, _new_name: &str) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }

    fn write(&mut self, node: NodeId, _offset: usize, data: &[u8]) -> Result<usize, FsError> {
        match Self::node(node).ok_or(FsError::NotFound)?.1 {
            DevNode::Device(kind, _) => kind.write(data.len()),
            DevNode::Root => Err(FsError::NotFile),
            DevNode::Link(_) => Err(FsError::NotPermitted),
        }
    }

    /// A device's length is always nothing, so `O_TRUNC` of one (every `> /dev/null`)
    /// has nothing to do.
    fn set_len(&mut self, node: NodeId, _size: usize) -> Result<(), FsError> {
        match Self::node(node).ok_or(FsError::NotFound)?.1 {
            DevNode::Device(..) => Ok(()),
            DevNode::Root => Err(FsError::NotFile),
            DevNode::Link(_) => Err(FsError::NotPermitted),
        }
    }

    fn flush(&mut self) -> Result<(), FsError> {
        Ok(())
    }

    fn set_attributes(&mut self, _node: NodeId, _attributes: u8) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }

    fn set_times(&mut self, _node: NodeId, _times: crate::NodeTimes) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }

    fn make_device(&mut self, _node: NodeId, _kind: DeviceKind) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }

    fn device(&self, node: NodeId) -> Option<DeviceKind> {
        match Self::node(node)?.1 {
            DevNode::Device(kind, _) => Some(kind),
            _ => None,
        }
    }

    fn posix(&self, node: NodeId) -> Option<PosixMeta> {
        let mode = match Self::node(node)?.1 {
            DevNode::Root => S_IFDIR | 0o755,
            DevNode::Device(_, mode) => S_IFCHR | mode,
            DevNode::Link(_) => S_IFLNK | 0o777,
        };
        Some(PosixMeta { mode, uid: 0, gid: 0, nlink: 1, ino: u64::from(node.0) + 1 })
    }

    fn set_mode(&mut self, _node: NodeId, _mode: u16) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }

    fn set_owner(&mut self, _node: NodeId, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }

    fn hard_link(&mut self, _node: NodeId, _new_parent: NodeId, _name: &str) -> Result<NodeId, FsError> {
        Err(FsError::NotPermitted)
    }

    fn keep_id(&mut self, _node: NodeId, _ino: u64, _keep: bool) -> Result<(), FsError> {
        Ok(())
    }
}

/// `/proc` itself.
const PROC_ROOT: u32 = 0;
/// `/proc/self`.
const PROC_SELF: u32 = 1;
/// `/proc/<pid>`, the caller's.
const PROC_PID: u32 = 2;
/// `/proc/<pid>/exe`.
const PROC_EXE: u32 = 3;
/// `/proc/<pid>/fd`.
const PROC_FD: u32 = 4;
/// `/proc/<pid>/fd/<n>` is this plus `n`.
const PROC_FD_BASE: u32 = 16;

/// What a `/proc` link whose target is longer than a link may be reads as. Relative and
/// with no slash in it, it leads nowhere, as a pipe's `pipe:[n]` does, so following a
/// descriptor's reaches the descriptor itself (the kernel's `descriptor_behind`).
pub const TOO_LONG: &str = "(too long)";

/// `/proc`: see the module's description. Node numbers are fixed, so a node's inode
/// number (its number plus one) is the same in every listing and every look.
pub struct ProcFs {
    view: &'static dyn ProcessView,
}

impl ProcFs {
    /// `/proc` as `view` sees the caller.
    pub const fn new(view: &'static dyn ProcessView) -> Self {
        Self { view }
    }

    fn fd_of(node: NodeId) -> Option<u32> {
        node.0.checked_sub(PROC_FD_BASE)
    }

    /// Where the link `node` leads, if it's one that's there for this caller. A target
    /// the tree wouldn't take for a link (longer than one may be) reads as [`TOO_LONG`], so the node
    /// is a link however long the path behind it: never a file the tree would open.
    fn link_target(&self, node: NodeId) -> Option<String> {
        let target = match node.0 {
            PROC_SELF => self.view.pid().map(|pid| pid.to_string()),
            PROC_EXE => self.view.pid().and(self.view.exe()),
            _ => self.view.descriptor_target(Self::fd_of(node)?),
        }?;
        if target.is_empty() || target.len() > MAX_LINK_TARGET {
            return Some(TOO_LONG.to_string());
        }
        Some(target)
    }

    fn is_directory(&self, node: NodeId) -> bool {
        matches!(node.0, PROC_ROOT) || (matches!(node.0, PROC_PID | PROC_FD) && self.view.pid().is_some())
    }
}

impl FileSystem for ProcFs {
    fn node_count(&self) -> usize {
        PROC_FD_BASE as usize + self.view.descriptor_count()
    }

    fn kind(&self, node: NodeId) -> Option<EntryKind> {
        if self.is_directory(node) {
            return Some(EntryKind::Directory);
        }
        if matches!(node.0, PROC_SELF | PROC_EXE) || node.0 >= PROC_FD_BASE {
            return self.link_target(node).map(|_| EntryKind::File);
        }
        None
    }

    fn name(&self, node: NodeId) -> Option<&str> {
        match node.0 {
            PROC_ROOT => Some("/"),
            PROC_SELF => Some("self"),
            PROC_EXE => Some("exe"),
            PROC_FD => Some("fd"),
            // Made up when asked: see `dynamic_name`.
            _ => None,
        }
    }

    fn parent(&self, node: NodeId) -> Option<NodeId> {
        match node.0 {
            PROC_ROOT => None,
            PROC_SELF | PROC_PID => Some(NodeId(PROC_ROOT)),
            PROC_EXE | PROC_FD => Some(NodeId(PROC_PID)),
            number if number >= PROC_FD_BASE => Some(NodeId(PROC_FD)),
            _ => None,
        }
    }

    fn size(&self, node: NodeId) -> usize {
        self.link_target(node).map_or(0, |target| SYMLINK_MAGIC.len() + target.len())
    }

    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize {
        self.try_read(node, offset, out).unwrap_or(0)
    }

    fn try_read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        if self.is_directory(node) {
            return Err(FsError::NotFile);
        }
        let target = self.link_target(node).ok_or(FsError::NotFound)?;
        Ok(read_link_bytes(&target, offset, out))
    }

    fn child_count(&self, node: NodeId) -> usize {
        if self.view.pid().is_none() {
            return 0;
        }
        match node.0 {
            PROC_ROOT | PROC_PID => 2,
            PROC_FD => self.view.descriptor_count(),
            _ => 0,
        }
    }

    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId> {
        self.view.pid()?;
        match (node.0, index) {
            (PROC_ROOT, 0) => Some(NodeId(PROC_SELF)),
            (PROC_ROOT, 1) => Some(NodeId(PROC_PID)),
            (PROC_PID, 0) => Some(NodeId(PROC_EXE)),
            (PROC_PID, 1) => Some(NodeId(PROC_FD)),
            (PROC_FD, _) => self.view.nth_descriptor(index).map(|fd| NodeId(PROC_FD_BASE + fd)),
            _ => None,
        }
    }

    /// Only the caller's own pid is there: any other is missing, not refused.
    fn lookup(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        let pid = self.view.pid()?;
        match (dir.0, name) {
            (PROC_ROOT, "self") => Some(NodeId(PROC_SELF)),
            (PROC_ROOT, _) => (parse_number(name)? == pid).then_some(NodeId(PROC_PID)),
            (PROC_PID, "exe") => Some(NodeId(PROC_EXE)),
            (PROC_PID, "fd") => Some(NodeId(PROC_FD)),
            (PROC_FD, _) => {
                let fd = parse_number(name)?;
                let node = NodeId(PROC_FD_BASE.checked_add(fd)?);
                self.view.descriptor_target(fd).map(|_| node)
            }
            _ => None,
        }
    }

    fn attributes(&self, node: NodeId) -> u8 {
        if self.is_directory(node) { 0 } else { attr::SYSTEM }
    }

    fn stats(&self) -> VolumeStats {
        VolumeStats { block_size: 4096, total_blocks: 0, free_blocks: 0, read_only: false, kind: "proc" }
    }
}

impl WritableFileSystem for ProcFs {
    fn create_file(&mut self, _parent: NodeId, _name: &str) -> Result<NodeId, FsError> {
        Err(FsError::NotPermitted)
    }
    fn create_dir(&mut self, _parent: NodeId, _name: &str) -> Result<NodeId, FsError> {
        Err(FsError::NotPermitted)
    }
    fn remove(&mut self, _node: NodeId) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }
    fn rename(&mut self, _node: NodeId, _new_parent: NodeId, _new_name: &str) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }
    fn write(&mut self, _node: NodeId, _offset: usize, _data: &[u8]) -> Result<usize, FsError> {
        Err(FsError::NotPermitted)
    }
    fn set_len(&mut self, _node: NodeId, _size: usize) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }
    fn flush(&mut self) -> Result<(), FsError> {
        Ok(())
    }
    fn set_attributes(&mut self, _node: NodeId, _attributes: u8) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }
    fn set_times(&mut self, _node: NodeId, _times: crate::NodeTimes) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }
    fn make_device(&mut self, _node: NodeId, _kind: DeviceKind) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }

    fn posix(&self, node: NodeId) -> Option<PosixMeta> {
        let mode = if self.is_directory(node) { S_IFDIR | 0o555 } else { S_IFLNK | 0o777 };
        Some(PosixMeta { mode, uid: 0, gid: 0, nlink: 1, ino: u64::from(node.0) + 1 })
    }

    fn set_mode(&mut self, _node: NodeId, _mode: u16) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }
    fn set_owner(&mut self, _node: NodeId, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), FsError> {
        Err(FsError::NotPermitted)
    }
    fn hard_link(&mut self, _node: NodeId, _new_parent: NodeId, _name: &str) -> Result<NodeId, FsError> {
        Err(FsError::NotPermitted)
    }
    fn keep_id(&mut self, _node: NodeId, _ino: u64, _keep: bool) -> Result<(), FsError> {
        Ok(())
    }

    fn dynamic_name(&self, node: NodeId) -> Option<String> {
        match node.0 {
            PROC_PID => self.view.pid().map(|pid| pid.to_string()),
            _ => Self::fd_of(node).map(|fd| fd.to_string()),
        }
    }

    /// Everything here but a directory is a link, or gone: one look at the view, so
    /// what the tree takes for the link is one target, never a short read of another.
    fn synthesised_link(&self, node: NodeId) -> Option<Result<Option<String>, FsError>> {
        if self.is_directory(node) {
            return Some(Ok(None));
        }
        Some(self.link_target(node).map(Some).ok_or(FsError::NotFound))
    }
}
