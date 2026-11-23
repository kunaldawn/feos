//! `/bin/sh`: feos-sh, the shell language the kernel's own shell runs, as a user program.
//!
//! The interpreter and the utilities (`libs/sh`, `libs/coreutils`) are the same code; what
//! this adds is the backend they run over here instead of the kernel's:
//!
//! - files through system calls: [`UserTree`] is the `feos_fs::VfsTree` the utilities'
//!   `OverlayFs` sees, where the kernel's shell has the kernel's shared tree;
//! - programs as processes: [`UserHost`] is a `feos_coreutils::Processes`, so a program
//!   gets its descriptors from the shell's through `SPAWN2`'s actions, pipelines of
//!   programs are real pipes, `exec` is `EXEC`, `wait` is `WAIT4` and `kill` is `KILL`;
//! - signals: `trap` handlers through `SIGACTION`, and with job control on a terminal each
//!   job's group is the terminal's foreground group while it runs (`TTY_SET_PGRP`, and
//!   `SPAWN2`'s `TCSETPGRP` action so a job has the terminal before it runs).
//!
//! It runs `-c` strings, scripts and standard input as the `sh` builtin does, and with no
//! operand on a terminal (or `-i`) reads commands at a prompt, a line at a time from the
//! terminal's own line discipline. Until relibc (phase 2), everything it needs of the
//! system is the small runtime in `user/src/lib.rs`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use feos_coreutils::process::errno::{EAGAIN, EINTR, ENOEXEC};
use feos_coreutils::process::{Action, Group, OpenMode, PollEntry, Processes, Spawn, WaitStatus};
use feos_coreutils::{Input, Output, System, Vfs};
use feos_fs::vfs::{DeviceKind, FileId, Handle, MountInfo, OpenOptions, Seek, TreeEntry};
use feos_fs::{EntryKind, FileKind, FileSystem, FsError, NodeId, NodeTimes, Stat, Timestamp, VfsTree, VolumeStats};
use feos_sh::{Host, Io, Shell};
use feos_user::abi::{self, at, errno, o, sig, spawn, spawn_action, Stat2, UserStr};
use feos_user::{entry, Args};

#[global_allocator]
static HEAP: feos_user::heap::UserHeap = feos_user::heap::UserHeap::new(256 << 20);

entry!(main);

/// The descriptor the shell keeps its terminal on, out of the way of what scripts use
/// and closed in everything it starts.
const TERMINAL_FD: usize = 63;

/// The signals that have arrived for a trap or Ctrl-C, as `1 << number`.
static PENDING: AtomicU32 = AtomicU32::new(0);

extern "C" fn note_signal(signal: i32, _info: *mut abi::SigInfo, _context: *mut abi::UContext) {
    if (1..32).contains(&signal) {
        PENDING.fetch_or(1 << signal, Ordering::SeqCst);
    }
}

/// A raw result as `Ok` or a positive error number.
fn check(result: isize) -> Result<usize, i32> {
    match abi::error_of(result) {
        Some(error) => Err(error as i32),
        None => Ok(result as usize),
    }
}

/// Linux's error numbers as the shared tree's errors.
fn fs_error(result: isize) -> FsError {
    match abi::error_of(result) {
        Some(errno::ENOENT) => FsError::NotFound,
        Some(errno::ENOTDIR) => FsError::NotDirectory,
        Some(errno::EISDIR) => FsError::IsDirectory,
        Some(errno::EEXIST) => FsError::NameExists,
        Some(errno::ENOTEMPTY) => FsError::NotEmpty,
        Some(errno::ENAMETOOLONG | errno::EINVAL | errno::EILSEQ) => FsError::InvalidName,
        Some(errno::ENOSPC | errno::EFBIG) => FsError::NoSpace,
        Some(errno::ENOMEM) => FsError::OutOfMemory,
        Some(errno::EROFS) => FsError::ReadOnly,
        Some(errno::EBUSY) => FsError::Busy,
        Some(errno::ELOOP) => FsError::SymlinkLoop,
        Some(errno::EACCES) => FsError::PermissionDenied,
        Some(errno::EPERM) => FsError::NotPermitted,
        Some(errno::ETXTBSY) => FsError::TextBusy,
        Some(errno::EXDEV) => FsError::CrossDevice,
        Some(errno::EOPNOTSUPP | errno::ENOSYS) => FsError::Unsupported,
        _ => FsError::Io,
    }
}

fn fs_check(result: isize) -> Result<usize, FsError> {
    if abi::error_of(result).is_some() {
        Err(fs_error(result))
    } else {
        Ok(result as usize)
    }
}

/// A descriptor closed when it goes.
struct Fd(usize);

impl Fd {
    fn open(path: &str, flags: usize, mode: usize) -> Result<Self, FsError> {
        fs_check(feos_user::openat(at::FDCWD, path, flags | o::CLOEXEC, mode)).map(Self)
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        feos_user::close(self.0);
    }
}

fn timestamp(time: abi::Timespec) -> Option<Timestamp> {
    (time.sec >= 0).then(|| Timestamp::from_parts(time.sec as u64, time.nsec.clamp(0, 999_999_999) as u32))
}

fn to_stat(stat: &Stat2) -> Stat {
    let kind = match stat.mode & abi::s_if::MASK {
        abi::s_if::DIR => FileKind::Directory,
        abi::s_if::LNK => FileKind::Symlink,
        _ => FileKind::File,
    };
    Stat {
        kind,
        size: stat.size.max(0) as u64,
        attributes: 0,
        times: NodeTimes {
            created: timestamp(stat.btime),
            modified: timestamp(stat.mtime),
            accessed: timestamp(stat.atime),
            changed: timestamp(stat.ctime),
        },
        read_only: false,
        mode: stat.mode as u16,
        uid: stat.uid,
        gid: stat.gid,
        nlink: stat.nlink,
        ino: stat.ino,
        dev: stat.dev,
        posix: true,
        device: None,
    }
}

fn timespec(time: Option<Timestamp>) -> abi::Timespec {
    match time {
        Some(time) => abi::Timespec { sec: (time.0 / Timestamp::NANOS) as i64, nsec: (time.0 % Timestamp::NANOS) as i64 },
        None => abi::Timespec { sec: 0, nsec: abi::utime::OMIT },
    }
}

/// The kernel's file tree, as the utilities' `OverlayFs` wants it: every path absolute,
/// every operation a system call or a few. Open handles, mounts and file identities
/// are the kernel's own business and aren't offered.
struct UserTree;

impl UserTree {
    fn stat_flags(path: &str, flags: usize) -> Result<Stat, FsError> {
        feos_user::fstatat(at::FDCWD, path, flags).map(|stat| to_stat(&stat)).map_err(fs_error)
    }
}

impl VfsTree for UserTree {
    fn stat(&self, path: &str) -> Result<Stat, FsError> {
        Self::stat_flags(path, 0)
    }

    fn lstat(&self, path: &str) -> Result<Stat, FsError> {
        Self::stat_flags(path, at::SYMLINK_NOFOLLOW)
    }

    fn statfs(&self, path: &str) -> Result<VolumeStats, FsError> {
        let fd = Fd::open(path, o::RDONLY, 0)?;
        let stats = feos_user::fstatvfs(fd.0).map_err(fs_error)?;
        Ok(VolumeStats {
            block_size: stats.frsize,
            total_blocks: stats.blocks,
            free_blocks: stats.bfree,
            read_only: stats.flags & 1 != 0,
            kind: "feos",
        })
    }

    fn read_dir(&self, path: &str) -> Result<Vec<TreeEntry>, FsError> {
        let fd = Fd::open(path, o::RDONLY | o::DIRECTORY, 0)?;
        let mut entries = Vec::new();
        let mut buffer = alloc::vec![0u8; 4096];
        loop {
            let length = fs_check(feos_user::getdents(fd.0, &mut buffer))?;
            if length == 0 {
                break;
            }
            let mut at = 0;
            while at + abi::Dirent::HEADER <= length {
                let record = usize::from(u16::from_ne_bytes([buffer[at + 16], buffer[at + 17]]));
                if record == 0 || at + record > length {
                    break;
                }
                let name = &buffer[at + abi::Dirent::HEADER..at + record];
                let name = &name[..name.iter().position(|byte| *byte == 0).unwrap_or(name.len())];
                at += record;
                let Ok(name) = core::str::from_utf8(name) else {
                    continue;
                };
                if name == "." || name == ".." {
                    continue;
                }
                let full = feos_coreutils::join(path, name);
                if let Ok(stat) = self.lstat(&full) {
                    entries.try_reserve(1).map_err(|_| FsError::OutOfMemory)?;
                    entries.push(TreeEntry { name: name.to_string(), stat });
                }
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let fd = Fd::open(path, o::RDONLY, 0)?;
        fs_check(feos_user::pread(fd.0, out, offset as usize))
    }

    fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        let fd = Fd::open(path, o::WRONLY, 0)?;
        fs_check(feos_user::pwrite(fd.0, data, offset as usize))
    }

    fn write_file(&self, path: &str, data: &[u8], append: bool) -> Result<(), FsError> {
        let how = if append { o::APPEND } else { o::TRUNC };
        let fd = Fd::open(path, o::WRONLY | o::CREAT | how, 0o666)?;
        let mut done = 0;
        while done < data.len() {
            match fs_check(feos_user::write(fd.0, &data[done..]))? {
                0 => return Err(FsError::NoSpace),
                count => done += count,
            }
        }
        Ok(())
    }

    fn write_file_atomic(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let temporary = alloc::format!("{path}.new");
        self.write_file(&temporary, data, false)?;
        fs_check(feos_user::renameat(at::FDCWD, &temporary, at::FDCWD, path, 0)).map(|_| ())
    }

    fn create_file(&self, path: &str) -> Result<(), FsError> {
        Fd::open(path, o::WRONLY | o::CREAT | o::EXCL, 0o666).map(|_| ())
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        fs_check(feos_user::mkdirat(at::FDCWD, path, 0o777)).map(|_| ())
    }

    fn remove_file(&self, path: &str) -> Result<(), FsError> {
        fs_check(feos_user::unlinkat(at::FDCWD, path, 0)).map(|_| ())
    }

    fn remove_dir(&self, path: &str) -> Result<(), FsError> {
        fs_check(feos_user::unlinkat(at::FDCWD, path, at::REMOVEDIR)).map(|_| ())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        fs_check(feos_user::renameat(at::FDCWD, from, at::FDCWD, to, 0)).map(|_| ())
    }

    fn truncate(&self, path: &str, size: u64) -> Result<(), FsError> {
        let fd = Fd::open(path, o::WRONLY, 0)?;
        fs_check(feos_user::ftruncate(fd.0, size as usize)).map(|_| ())
    }

    fn set_attributes(&self, _path: &str, _attributes: u8) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    fn set_times(&self, path: &str, times: NodeTimes) -> Result<(), FsError> {
        let times = [timespec(times.accessed), timespec(times.modified)];
        fs_check(feos_user::utimensat(at::FDCWD, path, Some(&times), 0)).map(|_| ())
    }

    fn symlink(&self, target: &str, path: &str) -> Result<(), FsError> {
        fs_check(feos_user::symlinkat(target, at::FDCWD, path)).map(|_| ())
    }

    fn read_link(&self, path: &str) -> Result<String, FsError> {
        let mut buffer = alloc::vec![0u8; abi::PATH_MAX];
        let length = fs_check(feos_user::readlinkat(at::FDCWD, path, &mut buffer))?;
        String::from_utf8(buffer[..length].to_vec()).map_err(|_| FsError::InvalidName)
    }

    fn open(&self, _path: &str, _options: OpenOptions) -> Result<Handle, FsError> {
        Err(FsError::Unsupported)
    }

    fn read_handle(&self, _handle: Handle, _out: &mut [u8]) -> Result<usize, FsError> {
        Err(FsError::Unsupported)
    }

    fn write_handle(&self, _handle: Handle, _data: &[u8]) -> Result<usize, FsError> {
        Err(FsError::Unsupported)
    }

    fn seek_handle(&self, _handle: Handle, _to: Seek) -> Result<u64, FsError> {
        Err(FsError::Unsupported)
    }

    fn stat_handle(&self, _handle: Handle) -> Result<Stat, FsError> {
        Err(FsError::Unsupported)
    }

    fn close(&self, _handle: Handle) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    fn sync(&self) -> Result<(), FsError> {
        fs_check(feos_user::sync()).map(|_| ())
    }

    fn sync_if_dirty(&self) -> Result<(), FsError> {
        Ok(())
    }

    fn generation(&self) -> u64 {
        0
    }

    fn set_change_hook(&self, _hook: fn()) {}

    fn mount(&self, _path: &str, _fs: Box<dyn feos_fs::WritableFileSystem>, _read_only: bool, _source: &str) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    fn unmount(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    fn mounts(&self) -> Vec<MountInfo> {
        Vec::new()
    }

    fn make_device(&self, _path: &str, _kind: DeviceKind) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    fn identify(&self, path: &str) -> Result<(FileId, Stat), FsError> {
        let stat = self.stat(path)?;
        Ok((FileId { dev: stat.dev, ino: stat.ino }, stat))
    }

    fn read_file(&self, _file: FileId, _offset: u64, _out: &mut [u8]) -> Result<usize, FsError> {
        Err(FsError::Unsupported)
    }

    fn stat_file(&self, _file: FileId) -> Result<Stat, FsError> {
        Err(FsError::Unsupported)
    }

    fn write_back(&self, _file: FileId, _offset: u64, _buffer: &mut [u8], _fill: &mut dyn FnMut(&mut [u8]) -> usize) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    fn hold(&self, _file: FileId) -> Result<(), FsError> {
        Ok(())
    }

    fn release(&self, _file: FileId) {}

    fn set_file_hook(&self, _hook: &'static dyn feos_fs::vfs::FileHook) {}

    fn chmod(&self, path: &str, mode: u16) -> Result<(), FsError> {
        fs_check(feos_user::fchmodat(at::FDCWD, path, usize::from(mode), 0)).map(|_| ())
    }

    fn chown(&self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
        let id = |id: Option<u32>| id.map_or(abi::KEEP, |id| id as usize);
        fs_check(feos_user::fchownat(at::FDCWD, path, id(uid), id(gid), 0)).map(|_| ())
    }

    fn link(&self, existing: &str, path: &str) -> Result<(), FsError> {
        fs_check(feos_user::linkat(at::FDCWD, existing, at::FDCWD, path, 0)).map(|_| ())
    }
}

static TREE: UserTree = UserTree;

/// What `OverlayFs` is made over: a filesystem with no nodes of its own whose tree is
/// [`UserTree`], so every operation goes there.
struct UserBase;

impl FileSystem for UserBase {
    fn node_count(&self) -> usize {
        0
    }

    fn kind(&self, _node: NodeId) -> Option<EntryKind> {
        None
    }

    fn name(&self, _node: NodeId) -> Option<&str> {
        None
    }

    fn parent(&self, _node: NodeId) -> Option<NodeId> {
        None
    }

    fn size(&self, _node: NodeId) -> usize {
        0
    }

    fn read(&self, _node: NodeId, _offset: usize, _out: &mut [u8]) -> usize {
        0
    }

    fn child_count(&self, _node: NodeId) -> usize {
        0
    }

    fn child_at(&self, _node: NodeId, _index: usize) -> Option<NodeId> {
        None
    }

    fn tree(&self) -> Option<&dyn VfsTree> {
        Some(&TREE)
    }
}

static BASE: UserBase = UserBase;

/// The facts `uname`, `id`, `$$` and `time` want.
struct UserSystem;

impl System for UserSystem {
    fn cpu_ms(&self) -> u64 {
        let ms = |time: abi::Timeval| (time.sec.max(0) as u64) * 1000 + (time.usec.max(0) as u64) / 1000;
        let mine = feos_user::getrusage(abi::rusage::SELF).map_or(0, |usage| ms(usage.utime) + ms(usage.stime));
        let children = feos_user::getrusage(abi::rusage::CHILDREN).map_or(0, |usage| ms(usage.utime) + ms(usage.stime));
        mine + children
    }

    fn machine(&self) -> &'static str {
        if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "x86_64"
        }
    }

    fn uid(&self) -> u32 {
        feos_user::getuid().max(0) as u32
    }

    fn gid(&self) -> u32 {
        feos_user::getgid().max(0) as u32
    }

    fn unix_time(&self) -> Option<u64> {
        feos_user::clock_gettime(abi::clock::REALTIME).ok().map(|time| time.sec.max(0) as u64)
    }

    fn uptime_ms(&self) -> u64 {
        feos_user::uptime_ms().max(0) as u64
    }

    fn clock_hz(&self) -> u64 {
        1000
    }

    fn pid(&self) -> u32 {
        feos_user::getpid().max(0) as u32
    }
}

static SYSTEM: UserSystem = UserSystem;

/// One of the shell's own descriptors as a command's input.
struct FdInput(usize);

impl Input for FdInput {
    fn read(&mut self, out: &mut [u8]) -> usize {
        loop {
            match check(feos_user::read(self.0, out)) {
                Ok(count) => return count,
                // A signal for a trap: the read goes on, and the trap runs after the command.
                Err(EINTR) if PENDING.load(Ordering::SeqCst) & 1 << sig::INT == 0 => {}
                Err(_) => return 0,
            }
        }
    }

    fn is_terminal(&self) -> bool {
        feos_user::isatty(self.0)
    }
}

/// One of the shell's own descriptors as a command's output.
struct FdOutput {
    fd: usize,
    broken: bool,
}

impl Output for FdOutput {
    fn write(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() && !self.broken {
            match check(feos_user::write(self.fd, bytes)) {
                Ok(0) => self.broken = true,
                Ok(count) => bytes = &bytes[count..],
                Err(EINTR | EAGAIN) => {}
                // `EPIPE` with `SIGPIPE` ignored, or a descriptor that's gone.
                Err(_) => self.broken = true,
            }
        }
    }

    fn is_terminal(&self) -> bool {
        feos_user::isatty(self.fd)
    }

    fn is_broken(&self) -> bool {
        self.broken
    }
}

/// The trap signals feos-sh knows, as `trap` names them.
const TRAPPABLE: [u32; 4] = [sig::HUP, sig::INT, sig::QUIT, sig::TERM];

/// What feos-sh runs programs and signals through.
struct UserHost {
    /// The terminal, kept on [`TERMINAL_FD`], when this shell does job control.
    terminal: Option<usize>,
    /// The shell's own group, the terminal's foreground group between jobs.
    group: u32,
    interactive: bool,
    /// The signals `trap` covers, as `1 << number`.
    trapped: u32,
    /// The highest descriptor past the shell's table it inherited and passes on.
    inherited_above: Option<i32>,
    /// The signals of [`IGNORED`] and [`TRAPPABLE`] that were ignored when the shell
    /// started, as `1 << (number - 1)`. They stay ignored for every program it starts, and
    /// in a shell without a prompt for the shell itself too, `trap` or not, as POSIX has it.
    ignored_on_entry: u64,
}

impl UserHost {
    /// Sets each trappable signal's disposition for what the traps and the shell's mode
    /// want: a handler for a trap; at a prompt Ctrl-C's `SIGINT` too (it cancels the
    /// line), and `SIGQUIT` and `SIGTERM` ignored, as Bash's are.
    fn set_dispositions(&self) {
        for signal in TRAPPABLE {
            let trapped = self.trapped & 1 << signal != 0;
            if !self.interactive && self.ignored_on_entry & 1 << (signal - 1) != 0 {
                // `nohup sh -c ...`: neither trapped nor reset (POSIX).
                feos_user::signal::set_disposition(signal, abi::sa::IGN);
            } else if trapped || (self.interactive && signal == sig::INT) {
                feos_user::signal::handle(signal, note_signal, 0);
            } else if self.interactive {
                feos_user::signal::set_disposition(signal, abi::sa::IGN);
            } else {
                feos_user::signal::set_disposition(signal, abi::sa::DFL);
            }
        }
    }

    fn set_foreground(&self, pgid: u32) {
        if let Some(fd) = self.terminal {
            feos_user::tcsetpgrp(fd, pgid as i32);
        }
    }
}

/// `UserStr`s for `strings`.
fn user_strs(strings: &[String]) -> Vec<UserStr> {
    strings.iter().map(|text| UserStr { pointer: text.as_ptr() as usize, length: text.len() }).collect()
}

fn open_flags(mode: OpenMode) -> u32 {
    (match mode {
        OpenMode::Read => o::RDONLY,
        OpenMode::Append => o::WRONLY | o::APPEND,
        OpenMode::ReadWrite => o::RDWR,
    }) as u32
}

/// The signals this shell may ignore of its own accord, which a program it starts takes
/// the defaults of (an ignored signal stays ignored across `EXEC`, unlike a handled one),
/// but for any of them the shell was started with ignored: [`UserHost::sigdefault`].
const IGNORED: u64 = 1 << (sig::PIPE - 1) | 1 << (sig::TTOU - 1) | 1 << (sig::TTIN - 1) | 1 << (sig::TSTP - 1) | 1 << (sig::QUIT - 1) | 1 << (sig::TERM - 1) | 1 << (sig::INT - 1);

impl UserHost {
    /// What a program this shell starts takes the default of: what the shell ignores of
    /// its own accord, not what it inherited ignored, which the program inherits in turn.
    fn sigdefault(&self) -> u64 {
        IGNORED & !self.ignored_on_entry
    }
}

/// [`UserHost::ignored_on_entry`], read before the shell sets any disposition.
fn ignored_on_entry() -> u64 {
    let mut ignored = 0;
    for signal in 1..32u32 {
        let watched = IGNORED & 1 << (signal - 1) != 0 || TRAPPABLE.contains(&signal);
        if watched && feos_user::signal::disposition(signal).is_ok_and(|action| action.handler == abi::sa::IGN) {
            ignored |= 1 << (signal - 1);
        }
    }
    ignored
}

impl Processes for UserHost {
    fn spawn(&mut self, request: &Spawn<'_>) -> Result<u32, i32> {
        let mut actions = Vec::with_capacity(request.actions.len() + 2);
        // The group's the terminal's before the descriptors move, while the terminal is
        // still on its own descriptor; and the working directory is the shell's.
        if request.foreground {
            if let Some(fd) = self.terminal {
                actions.push(abi::SpawnAction { kind: spawn_action::TCSETPGRP, fd: fd as i32, ..Default::default() });
            }
        }
        actions.push(abi::SpawnAction { kind: spawn_action::CHDIR, path: feos_user::path_at(at::FDCWD, request.cwd), ..Default::default() });
        for action in request.actions {
            actions.push(match action {
                Action::Dup2 { from, to } => abi::SpawnAction { kind: spawn_action::DUP2, fd: *from, to: *to, ..Default::default() },
                Action::Open { fd, path, mode } => abi::SpawnAction {
                    kind: spawn_action::OPEN,
                    fd: *fd,
                    open_flags: open_flags(*mode),
                    mode: 0o666,
                    path: feos_user::path_at(at::FDCWD, path),
                    ..Default::default()
                },
                Action::Close(fd) => abi::SpawnAction { kind: spawn_action::CLOSE, fd: *fd, ..Default::default() },
                Action::CloseFrom(fd) => abi::SpawnAction { kind: spawn_action::CLOSEFROM, fd: *fd, ..Default::default() },
            });
        }
        let argv = user_strs(request.argv);
        let envp = user_strs(request.env);
        let mut spawn = feos_user::spawn_request(request.path, &argv, &envp, &actions);
        spawn.flags = spawn::SETSIGDEF | spawn::SETSIGMASK;
        spawn.sigdefault = self.sigdefault();
        match request.group {
            Group::Inherit => {}
            Group::New => spawn.flags |= spawn::SETPGROUP,
            Group::Join(pgid) => {
                spawn.flags |= spawn::SETPGROUP;
                spawn.pgroup = i64::from(pgid);
            }
        }
        check(feos_user::spawn2(&spawn)).map(|pid| pid as u32)
    }

    fn exec(&mut self, request: &Spawn<'_>) -> i32 {
        for action in request.actions {
            let result = match action {
                Action::Dup2 { from, to } if from == to => feos_user::fcntl(*from as usize, abi::f::SETFD, 0),
                Action::Dup2 { from, to } => feos_user::dup3(*from as usize, *to as usize, 0),
                Action::Open { fd, path, mode } => {
                    let opened = feos_user::openat(at::FDCWD, path, open_flags(*mode) as usize, 0o666);
                    match check(opened) {
                        Ok(opened) if opened == *fd as usize => 0,
                        Ok(opened) => {
                            let moved = feos_user::dup3(opened, *fd as usize, 0);
                            feos_user::close(opened);
                            moved
                        }
                        Err(_) => opened,
                    }
                }
                Action::Close(fd) => {
                    feos_user::close(*fd as usize);
                    0
                }
                Action::CloseFrom(fd) => feos_user::close_range(*fd as usize, u32::MAX as usize, 0),
            };
            if let Err(error) = check(result) {
                return error;
            }
        }
        if let Ok(directory) = Fd::open(request.cwd, o::RDONLY | o::DIRECTORY, 0) {
            feos_user::fchdir(directory.0);
        }
        // What this shell ignores or handles goes back to the defaults the program
        // expects (a handled one does by itself, an ignored one wouldn't), and nothing's
        // blocked.
        for signal in 1..32 {
            if self.sigdefault() & 1 << (signal - 1) != 0 {
                feos_user::signal::set_disposition(signal, abi::sa::DFL);
            }
        }
        feos_user::signal::sigprocmask(abi::how::SETMASK, Some(0), None);
        let argv = user_strs(request.argv);
        let envp = user_strs(request.env);
        let error = feos_user::exec(&feos_user::exec_request(request.path, &argv, &envp));
        // Still here: the shell's own handling comes back for its last words.
        feos_user::signal::set_disposition(sig::PIPE, abi::sa::IGN);
        self.set_dispositions();
        abi::error_of(error).map_or(ENOEXEC, |error| error as i32)
    }

    fn wait(&mut self, pid: u32, block: bool) -> Result<Option<WaitStatus>, i32> {
        let mut options = if block { 0 } else { abi::wait::NOHANG };
        if self.terminal.is_some() {
            options |= abi::wait::UNTRACED;
            if !block {
                options |= abi::wait::CONTINUED;
            }
        }
        match feos_user::wait4(pid as isize, options) {
            Ok((0, _)) => Ok(None),
            Ok((_, status)) => Ok(Some(WaitStatus::from_raw(status))),
            Err(error) => Err(abi::error_of(error).unwrap_or(errno::EINVAL) as i32),
        }
    }

    fn pipe(&mut self) -> Result<(i32, i32), i32> {
        feos_user::pipe2(o::CLOEXEC)
            .map(|(read, write)| (read as i32, write as i32))
            .map_err(|error| abi::error_of(error).unwrap_or(errno::EMFILE) as i32)
    }

    fn set_nonblocking(&mut self, fd: i32) {
        let flags = feos_user::fcntl(fd as usize, abi::f::GETFL, 0);
        if flags >= 0 {
            feos_user::fcntl(fd as usize, abi::f::SETFL, flags as usize | o::NONBLOCK);
        }
    }

    fn close(&mut self, fd: i32) {
        feos_user::close(fd as usize);
    }

    fn read(&mut self, fd: i32, out: &mut [u8]) -> Result<usize, i32> {
        check(feos_user::read(fd as usize, out))
    }

    fn write(&mut self, fd: i32, bytes: &[u8]) -> Result<usize, i32> {
        check(feos_user::write(fd as usize, bytes))
    }

    fn poll(&mut self, entries: &mut [PollEntry], timeout_ms: i32) -> Result<usize, i32> {
        let mut fds: Vec<abi::PollFd> = entries
            .iter()
            .map(|entry| {
                let mut events = 0;
                if entry.read {
                    events |= abi::poll::IN;
                }
                if entry.write {
                    events |= abi::poll::OUT;
                }
                abi::PollFd { fd: entry.fd, events, revents: 0 }
            })
            .collect();
        let timeout = if timeout_ms < 0 { -1 } else { i64::from(timeout_ms) * 1_000_000 };
        let count = check(feos_user::poll(&mut fds, timeout, None))?;
        for (entry, fd) in entries.iter_mut().zip(&fds) {
            entry.ready = fd.revents != 0;
        }
        Ok(count)
    }

    fn signal(&mut self, pid: i32, signal: u32) -> Result<(), i32> {
        check(feos_user::kill(pid as isize, signal)).map(|_| ())
    }

    fn inherited_above(&self) -> Option<i32> {
        self.inherited_above
    }

    fn job_control(&self) -> bool {
        self.terminal.is_some()
    }

    fn give_terminal(&mut self, pgid: u32) {
        self.set_foreground(pgid);
    }

    fn take_terminal(&mut self) {
        self.set_foreground(self.group);
    }
}

impl Host for UserHost {
    fn run_external(
        &mut self,
        _name: &str,
        _args: &[String],
        _stdin: &mut dyn Input,
        _stdout: &mut dyn Output,
        _stderr: &mut dyn Output,
        _background: bool,
    ) -> Option<i32> {
        None
    }

    fn poll_cancel(&mut self) -> bool {
        // Ctrl-C at the shell itself (not a job, which has the terminal then) stops what
        // it's running, unless a trap wants the signal.
        let interrupt = 1 << sig::INT;
        self.interactive && self.trapped & interrupt == 0 && PENDING.fetch_and(!interrupt, Ordering::SeqCst) & interrupt != 0
    }

    fn catch_signals(&mut self, mask: u32) {
        self.trapped = mask;
        self.set_dispositions();
    }

    fn take_signals(&mut self) -> u32 {
        PENDING.fetch_and(!self.trapped, Ordering::SeqCst) & self.trapped
    }

    fn processes(&mut self) -> Option<&mut dyn Processes> {
        Some(self)
    }
}

/// The descriptors above 2 this shell was started with open and not close-on-exec,
/// which POSIX has every program it runs inherit (GNU make hands its jobserver's pipe to
/// sub-makes through `sh -c` this way). Every descriptor the shell opens of its own is
/// close-on-exec, so this is looked at once, before it has any.
fn inherited_descriptors() -> Vec<usize> {
    let mut found: Vec<usize> = TREE
        .read_dir("/proc/self/fd")
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry.name.parse().ok())
        .filter(|fd| *fd > 2 && feos_user::fcntl(*fd, abi::f::GETFD, 0) == 0)
        .collect();
    found.sort_unstable();
    found
}

/// What reading a line at the prompt found.
enum Line {
    Text(String),
    Interrupted,
    End,
}

/// Reads one line from standard input, as the terminal's line discipline gives it.
fn read_line(pending: &mut Vec<u8>) -> Line {
    loop {
        if let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            return Line::Text(String::from_utf8_lossy(&line[..end]).into_owned());
        }
        let mut buffer = [0u8; 512];
        match check(feos_user::read(0, &mut buffer)) {
            Ok(0) if pending.is_empty() => return Line::End,
            Ok(0) => {
                let line = core::mem::take(pending);
                return Line::Text(String::from_utf8_lossy(&line).into_owned());
            }
            Ok(count) => pending.extend_from_slice(&buffer[..count]),
            Err(EINTR) => {
                if PENDING.fetch_and(!(1 << sig::INT), Ordering::SeqCst) & 1 << sig::INT != 0 {
                    pending.clear();
                    return Line::Interrupted;
                }
            }
            Err(_) => return Line::End,
        }
    }
}

/// Reads commands at a prompt until end of input or `exit`.
fn interactive(shell: &mut Shell<'_>, io: &mut Io<'_>, host: &mut UserHost) -> i32 {
    let mut pending = Vec::new();
    loop {
        shell.poll_signals(io, host);
        if let Some(status) = shell.take_exit_status() {
            return status;
        }
        let mut text = String::new();
        let mut prompt = shell.prompt();
        loop {
            io.stderr.write(prompt.as_bytes());
            match read_line(&mut pending) {
                Line::Text(line) => {
                    text.push_str(&line);
                    text.push('\n');
                    // A line that leaves an `if` or a quote open goes on at `PS2`.
                    if feos_sh::parser::check_complete(&text) == Err(true) {
                        prompt = shell.continuation_prompt();
                        continue;
                    }
                    break;
                }
                Line::Interrupted => {
                    io.stderr.write(b"\n");
                    text.clear();
                    break;
                }
                Line::End => {
                    if !text.is_empty() {
                        break;
                    }
                    io.stderr.write(b"\n");
                    return shell.last_status;
                }
            }
        }
        if text.trim().is_empty() {
            continue;
        }
        let line = text.trim_end_matches('\n');
        feos_sh::remember(shell, line);
        shell.execute_line(line, io, host);
        if let Some(status) = shell.take_exit_status() {
            return status;
        }
    }
}

/// Whether `sh`'s arguments ask for a prompt: no `-c`, no script operand, and `-i` or
/// standard input a terminal.
fn wants_prompt(args: &[String]) -> bool {
    let mut forced = false;
    let mut from_stdin = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            index += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').or_else(|| arg.strip_prefix('+')).filter(|flags| !flags.is_empty()) else {
            break;
        };
        for flag in flags.chars() {
            match flag {
                'c' => return false,
                'i' => forced = true,
                's' => from_stdin = true,
                'o' => index += 1,
                _ => {}
            }
        }
        index += 1;
    }
    if index < args.len() && !from_stdin {
        return false;
    }
    forced || feos_user::isatty(0)
}

fn main(args: Args) -> i32 {
    // A write to a pipe whose reader has gone is an error to report, not the end of the
    // shell: the programs it starts get `SIGPIPE`'s default back (unless it came ignored).
    let ignored_on_entry = ignored_on_entry();
    feos_user::signal::set_disposition(sig::PIPE, abi::sa::IGN);
    let inherited = inherited_descriptors();

    let mut shell = Shell::new(Some(&BASE), &SYSTEM);
    let mut inherited_above = None;
    for fd in inherited {
        if fd < 10 {
            shell.inherit_descriptor(fd);
        } else {
            inherited_above = Some(fd as i32);
        }
    }
    // Each recursive path is bounded by this: the main stack is 8 MiB, of which the
    // interpreter may use half, leaving room for the frames of what it runs.
    shell.set_stack_limit(4 << 20);
    for entry in feos_user::environment() {
        let Ok(entry) = core::str::from_utf8(entry) else {
            continue;
        };
        if let Some((name, value)) = entry.split_once('=') {
            shell.env.set(name, value);
            shell.env.export(name);
        }
    }
    let mut cwd = alloc::vec![0u8; abi::PATH_MAX];
    if let Ok(length) = check(feos_user::getcwd(&mut cwd)) {
        if let Ok(cwd) = core::str::from_utf8(&cwd[..length]) {
            let cwd = cwd.trim_end_matches('\0').to_string();
            if shell.fs.chdir(&cwd).is_ok() {
                shell.env.set("PWD", &cwd);
            }
        }
    }

    let words: Vec<String> = (1..args.len()).map(|index| args.get(index).unwrap_or("").to_string()).collect();
    let prompt = wants_prompt(&words);
    let mut host = UserHost { terminal: None, group: feos_user::getpgid(0).max(0) as u32, interactive: prompt, trapped: 0, inherited_above, ignored_on_entry };
    if prompt && feos_user::isatty(0) {
        // Job control: the shell leads a group of its own, which has the terminal, kept
        // on a descriptor of its own so `exec <file` can't take it away.
        feos_user::setpgid(0, 0);
        host.group = feos_user::getpgid(0).max(0) as u32;
        let kept = feos_user::fcntl(0, abi::f::DUPFD_CLOEXEC, TERMINAL_FD);
        if let Ok(fd) = check(kept) {
            host.terminal = Some(fd);
            host.set_foreground(host.group);
            for signal in [sig::TSTP, sig::TTIN, sig::TTOU] {
                feos_user::signal::set_disposition(signal, abi::sa::IGN);
            }
        }
    }
    host.set_dispositions();

    let mut stdin = FdInput(0);
    let mut stdout = FdOutput { fd: 1, broken: false };
    let mut stderr = FdOutput { fd: 2, broken: false };
    let mut io = Io { stdin: &mut stdin, stdout: &mut stdout, stderr: &mut stderr };
    if prompt {
        shell.set_interactive(true);
        let status = interactive(&mut shell, &mut io, &mut host);
        // End of input leaves as `exit` would, the EXIT trap included.
        return shell.execute_line(&alloc::format!("exit {status}"), &mut io, &mut host);
    }
    shell.set_interactive(false);
    shell.run_arguments(&words, &mut io, &mut host)
}
