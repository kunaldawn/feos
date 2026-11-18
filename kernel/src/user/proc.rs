//! The process calls of stage 1c.6: `SPAWN2` (and v1's `SPAWN` beside it), process
//! groups and sessions, `GETRUSAGE` and `SYSINFO`.
//!
//! `SPAWN2` does in the call everything that can fail, before the child exists, so every
//! failure is the call's result and none a child's exit status (section 3.4 of the ABI
//! design): the request and its arrays are copied once (the strings into staging frames,
//! see [`load::Staged`]), the child's descriptor table is built from a copy of the
//! caller's by the actions, its program is resolved and held, and its exit record taken
//! in the caller. Then the task is made, and builds its address space itself.

use alloc::string::String;
use alloc::vec::Vec;

use feos_abi::{at, errno, int_arg, o, rusage, spawn, spawn_action as kind, sysinfo, Rusage, SpawnAction, SpawnRequest, SysInfo, Timeval, UserStr, MAX_FDS, PAGE_SIZE};
use feos_app::{ArgBlock, SpawnError, PROCESS_ARGS_MAX};
use feos_fs::vfs::{path, ACCESS_EXECUTE};
use feos_fs::FileKind;
use feos_vm::FrameSource;

use super::files::{self, Files, Object, OfdRef, Request};
use super::fs_calls::{self, errno_of};
use super::load::{self, Staged, UserStart};
use super::memory_calls::{KernelFrames, PROMISES};
use super::process::Process;
use super::tree;
use crate::sched::{self, RawStreams, SpawnFail, SpawnGroup};
use crate::{memory, time};

/// Signals no mask may hold: `SIGKILL` and `SIGSTOP`.
const UNBLOCKABLE: u64 = 1 << (9 - 1) | 1 << (19 - 1);

/// What a failed spawn is called across the boundary, for `SPAWN2`.
fn spawn_errno(fail: SpawnFail) -> isize {
    match fail {
        SpawnFail::Spawn(SpawnError::NoFreeSlot) => -errno::EAGAIN,
        SpawnFail::Spawn(SpawnError::NoMemory) => -errno::ENOMEM,
        SpawnFail::Spawn(SpawnError::ArgsTooLong) => -errno::E2BIG,
        SpawnFail::Spawn(SpawnError::Unsupported) => -errno::EINVAL,
        SpawnFail::Errno(error) => error,
    }
}

/// The last component of a path, as a task's name.
fn name_of(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// `spawn2`.
pub fn sys_spawn2(slot: usize, process: &Process, address: usize) -> isize {
    match spawn2(slot, process, address) {
        Ok(pid) => pid as isize,
        Err(error) => error,
    }
}

/// The request's own fields, before anything is copied: reserved fields zero, flags
/// known, the fields a flag doesn't ask for zero, and counts within their limits.
fn check_request(request: &SpawnRequest) -> Result<(), isize> {
    let flags = request.flags;
    let unasked = |flag: u64, value: u64| flags & flag == 0 && value != 0;
    if request.reserved != [0; 3]
        || request.path.reserved != 0
        || flags & !spawn::ALL != 0
        || unasked(spawn::SETPGROUP, request.pgroup as u64)
        || unasked(spawn::SETSIGMASK, request.sigmask)
        || unasked(spawn::SETSIGDEF, request.sigdefault)
        // A new session's leader can't then join a group.
        || flags & (spawn::SETSID | spawn::SETPGROUP) == spawn::SETSID | spawn::SETPGROUP
        || !(0..=i64::from(i32::MAX)).contains(&request.pgroup)
        || (flags & spawn::EMPTY_PATH != 0 && request.path.length != 0)
    {
        return Err(-errno::EINVAL);
    }
    if request.argc == 0 || request.action_count > spawn::MAX_ACTIONS as u64 {
        return Err(-errno::EINVAL);
    }
    if request.argc.checked_add(request.envc).is_none_or(|count| count > spawn::MAX_STRINGS as u64) {
        return Err(-errno::E2BIG);
    }
    Ok(())
}

/// The argument and environment strings, copied once: first their `UserStr` arrays,
/// whole, then each string by the lengths in that copy, so a length another thread
/// changes meanwhile is either the one checked or never seen.
fn stage_strings(process: &Process, request: &SpawnRequest) -> Result<Staged, isize> {
    let user = process.mm();
    let (argc, envc) = (request.argc as usize, request.envc as usize);
    let size = core::mem::size_of::<UserStr>();
    let mut table = Staged::new((argc + envc) * size, Some(process))?;
    table.append_user(user, request.argv as usize, argc * size)?;
    table.append_user(user, request.envp as usize, envc * size)?;
    // Strings and their pointers together, within `ARG_MAX`, each within `STRING_MAX`.
    let mut total = (argc + envc) * 8;
    for index in 0..argc + envc {
        let length = table.word(index * size + 8) as usize;
        if length > spawn::STRING_MAX {
            return Err(-errno::E2BIG);
        }
        total += length + 1;
        if total > spawn::ARG_MAX {
            return Err(-errno::E2BIG);
        }
    }
    let mut strings = Staged::new(total - (argc + envc) * 8, Some(process))?;
    for index in 0..argc + envc {
        let pointer = table.word(index * size) as usize;
        let length = table.word(index * size + 8) as usize;
        strings.push_user(user, pointer, length, index >= argc)?;
    }
    Ok(strings)
}

/// The actions, copied once, each checked: a kind there is, and nothing in the fields it
/// doesn't use.
fn copy_actions(process: &Process, request: &SpawnRequest) -> Result<Vec<SpawnAction>, isize> {
    let count = request.action_count as usize;
    let mut actions = Vec::new();
    if count == 0 {
        return Ok(actions);
    }
    let _charge = process.charge_heap(count * (SpawnAction::SIZE + core::mem::size_of::<SpawnAction>()))?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(count * SpawnAction::SIZE).map_err(|_| -errno::ENOMEM)?;
    raw.resize(count * SpawnAction::SIZE, 0);
    process.mm().copy_in(request.actions as usize, &mut raw)?;
    actions.try_reserve_exact(count).map_err(|_| -errno::ENOMEM)?;
    for chunk in raw.as_chunks::<{ SpawnAction::SIZE }>().0 {
        let action = SpawnAction::from_bytes(chunk);
        let path = action.path != feos_abi::PathAt::default();
        let fine = action.reserved == 0
            && action.path.reserved == 0
            && match action.kind {
                kind::CLOSE | kind::FCHDIR | kind::CLOSEFROM | kind::TCSETPGRP => {
                    action.to == 0 && action.open_flags == 0 && action.mode == 0 && !path
                }
                kind::DUP2 => action.open_flags == 0 && action.mode == 0 && !path,
                kind::OPEN => action.to == 0,
                kind::CHDIR => action.fd == 0 && action.to == 0 && action.open_flags == 0 && action.mode == 0,
                _ => false,
            };
        if !fine {
            return Err(-errno::EINVAL);
        }
        actions.push(action);
    }
    Ok(actions)
}

/// A descriptor of an action: `EBADF` outside `0..MAX_FDS`.
fn action_fd(fd: i32) -> Result<usize, isize> {
    usize::try_from(fd).ok().filter(|&fd| fd < MAX_FDS).ok_or(-errno::EBADF)
}

/// `given` made absolute against the child's table: its working directory for
/// `AT_FDCWD`, or the directory `dirfd` refers to there.
fn join_child(files: &Files, dirfd: i32, given: &str) -> Result<String, isize> {
    if given.starts_with('/') {
        return path::join("/", given).map_err(errno_of);
    }
    if dirfd == at::FDCWD {
        return path::join(files.cwd(), given).map_err(errno_of);
    }
    let ofd = usize::try_from(dirfd).ok().and_then(|fd| files.table.get(fd)).ok_or(-errno::EBADF)?;
    match &ofd.object {
        Object::Directory { path, .. } => path::join(path, given).map_err(errno_of),
        _ => Err(-errno::ENOTDIR),
    }
}

/// An `OPEN` action's flags, as `OPENAT` takes them.
fn open_request(flags: u32, mode: u32) -> Result<Request, isize> {
    let flags = flags as usize;
    if flags & o::TMPFILE == o::TMPFILE {
        return Err(-errno::EOPNOTSUPP);
    }
    if flags & !o::OPENAT_KNOWN != 0 || flags & o::ACCMODE == o::ACCMODE || flags & (o::CREAT | o::DIRECTORY) == o::CREAT | o::DIRECTORY {
        return Err(-errno::EINVAL);
    }
    let path_only = flags & o::PATH != 0;
    Ok(Request {
        access: flags & o::ACCMODE,
        create: flags & o::CREAT != 0 && !path_only,
        exclusive: flags & o::EXCL != 0,
        truncate: flags & o::TRUNC != 0 && !path_only,
        status: flags & o::SETTABLE,
        directory: flags & o::DIRECTORY != 0,
        nofollow: flags & o::NOFOLLOW != 0,
        path_only,
        sync: flags & o::DSYNC != 0,
        cloexec: flags & o::CLOEXEC != 0,
        mode,
    })
}

/// Makes the directory `joined` the child's working directory, if this user may search it.
fn change_dir(slot: usize, files: &mut Files, joined: &str) -> Result<(), isize> {
    let tree = tree().ok_or(-errno::EIO)?;
    let found = path::lookup(tree, joined, true).map_err(errno_of)?;
    if found.stat.kind != FileKind::Directory {
        return Err(-errno::ENOTDIR);
    }
    let (uid, gid) = sched::credentials(slot);
    tree.check_access(&found.canonical, uid, gid, ACCESS_EXECUTE).map_err(errno_of)?;
    drop(files.set_cwd(found.canonical));
    Ok(())
}

/// Runs one action against the child's table, its working directory and umask. What
/// it closes goes in `closed`, to be let go once the table is done with.
fn run_action(slot: usize, process: &Process, files: &mut Files, action: &SpawnAction, closed: &mut Vec<OfdRef>) -> Result<(), isize> {
    let mut keep = |ofd: Option<OfdRef>| -> Result<(), isize> {
        if let Some(ofd) = ofd {
            closed.try_reserve(1).map_err(|_| -errno::ENOMEM)?;
            closed.push(ofd);
        }
        Ok(())
    };
    match action.kind {
        // One that isn't open is ignored, as glibc and musl do.
        kind::CLOSE => {
            let fd = action_fd(action.fd)?;
            keep(files.table.remove(fd).ok())
        }
        kind::DUP2 => {
            let (from, to) = (action_fd(action.fd)?, action_fd(action.to)?);
            if files.table.get(from).is_none() {
                return Err(-errno::EBADF);
            }
            // The same descriptor passes through: its close-on-exec flag goes.
            if from == to {
                return files.table.set_cloexec(to, false).map_err(|_| -errno::EBADF);
            }
            match files.table.dup3(from, to, false) {
                Ok(before) => keep(before),
                Err(feos_fdtable::FdError::NoMemory) => Err(-errno::ENOMEM),
                Err(_) => Err(-errno::EBADF),
            }
        }
        kind::OPEN => {
            let fd = action_fd(action.fd)?;
            let request = open_request(action.open_flags, action.mode)?;
            let given = fs_calls::user_path(process, action.path.pointer as usize, action.path.length as usize)?;
            let joined = join_child(files, action.path.dirfd, &given)?;
            let ofd = files::open_description(slot, process, &joined, &request, files.umask).map_err(fs_calls::failed)?;
            match files.table.put(fd, ofd, request.cloexec) {
                Ok(before) => keep(before),
                Err((_, ofd)) => {
                    drop(ofd);
                    Err(-errno::ENOMEM)
                }
            }
        }
        kind::CHDIR => {
            let given = fs_calls::user_path(process, action.path.pointer as usize, action.path.length as usize)?;
            let joined = join_child(files, action.path.dirfd, &given)?;
            change_dir(slot, files, &joined)
        }
        kind::FCHDIR => {
            let fd = action_fd(action.fd)?;
            let ofd = files.table.get(fd).cloned().ok_or(-errno::EBADF)?;
            let Object::Directory { path, .. } = &ofd.object else {
                return Err(-errno::ENOTDIR);
            };
            let joined = path::join("/", path).map_err(errno_of)?;
            change_dir(slot, files, &joined)
        }
        kind::CLOSEFROM => {
            let fd = action_fd(action.fd)?;
            let mut failed = Ok(());
            let result = files.table.close_range(fd, MAX_FDS - 1, false, |ofd| {
                if failed.is_ok() {
                    failed = keep(Some(ofd));
                }
            });
            result.map_err(|_| -errno::EBADF)?;
            failed
        }
        // A terminal's foreground group is stage 1c.11's, which has terminals.
        _ => Err(-errno::EINVAL),
    }
}

fn spawn2(slot: usize, process: &Process, address: usize) -> Result<u32, isize> {
    let mut bytes = [0u8; SpawnRequest::SIZE];
    process.mm().copy_in(address, &mut bytes)?;
    let request = SpawnRequest::from_bytes(&bytes);
    check_request(&request)?;
    let empty_path = request.flags & spawn::EMPTY_PATH != 0;
    let given = if empty_path { None } else { Some(fs_calls::user_path(process, request.path.pointer as usize, request.path.length as usize)?) };
    let strings = stage_strings(process, &request)?;
    let actions = copy_actions(process, &request)?;

    // The child's table: the caller's, then the actions in order, then whatever is to
    // close on exec closed. What closes is let go here, with no table held.
    let mut files = process.files().try_clone()?;
    let mut closed = Vec::new();
    for action in &actions {
        run_action(slot, process, &mut files, action, &mut closed)?;
    }
    drop(closed);
    drop(files.table.cloexec_sweep());

    // The program, against the child's table and working directory.
    let program = match &given {
        Some(given) => join_child(&files, request.path.dirfd, given)?,
        None => {
            let ofd = usize::try_from(request.path.dirfd).ok().and_then(|fd| files.table.get(fd)).ok_or(-errno::EBADF)?;
            match &ofd.object {
                Object::File { path, .. } => load::copy(path).ok_or(-errno::ENOMEM)?,
                _ => return Err(-errno::EACCES),
            }
        }
    };
    let tree = tree().ok_or(-errno::EIO)?;
    let (uid, gid) = sched::credentials(slot);
    let image = load::resolve(tree, &program, uid, gid).map_err(|refused| refused.errno)?;
    let name = load::copy(name_of(&image.canonical)).ok_or(-errno::ENOMEM)?;

    let group = if request.flags & spawn::SETSID != 0 {
        SpawnGroup::NewSession
    } else if request.flags & spawn::SETPGROUP != 0 {
        match request.pgroup {
            0 => SpawnGroup::Own,
            pgroup => SpawnGroup::Join(pgroup as u32),
        }
    } else {
        SpawnGroup::Inherit
    };
    // Handlers aren't kept by a new image, and none exist yet to reset (stage 1c.9), so
    // `SETSIGDEF` has nothing more to do.
    let sigmask = (request.flags & spawn::SETSIGMASK != 0).then_some(request.sigmask & !UNBLOCKABLE);
    let raw = RawStreams { stdin: 0, stdout: 0, umask: Some(files.umask), group, sigmask };
    let shown = strings.shown();
    let given = given.unwrap_or_default();
    let cwd = load::copy(files.cwd()).ok_or(-errno::ENOMEM)?;
    let umask = files.umask;

    process.reserve_child()?;
    let Some(start) = UserStart::make(&program, Some(image), if given.is_empty() { &program } else { &given }, &cwd, umask, strings, Some(files)) else {
        process.child_failed();
        return Err(-errno::ENOMEM);
    };
    match sched::spawn_program(&name, start, shown, raw) {
        Ok(pid) => {
            process.child_made(pid);
            Ok(pid)
        }
        Err(fail) => {
            process.child_failed();
            Err(spawn_errno(fail))
        }
    }
}

/// v1's `spawn`: copies everything from the program before anything happens, and keeps
/// v1's errors. The child's table is its standard streams, and it resolves its program
/// itself, so one that isn't there ends with 127.
pub fn sys_spawn(process: &Process, args: [usize; 6]) -> isize {
    let [path_pointer, path_length, argv, argc, stdin_fd, stdout_fd] = args;
    let path = match super::user_path(process, path_pointer, path_length) {
        Ok(path) => path,
        Err(error) => return error,
    };
    // Nor is anything run from there.
    if let Some(tree) = tree() {
        if let Err(error) = super::check_visible(tree, &path, true) {
            return error;
        }
    }
    if argc > feos_abi::MAX_ARGS {
        return -errno::ENAMETOOLONG;
    }
    let mut block = ArgBlock::new();
    let mut scratch = [0u8; PROCESS_ARGS_MAX];
    for index in 0..argc {
        let Some(at) = index.checked_mul(core::mem::size_of::<UserStr>()).and_then(|offset| argv.checked_add(offset)) else {
            return -errno::EFAULT;
        };
        let mut raw = [0u8; 16];
        if process.mm().copy_in(at, &mut raw).is_err() {
            return -errno::EFAULT;
        }
        let mut word = [0u8; 8];
        word.copy_from_slice(&raw[..8]);
        let pointer = usize::from_ne_bytes(word);
        word.copy_from_slice(&raw[8..]);
        let length = usize::from_ne_bytes(word);
        if length != 0 && !process.mm().in_window(pointer, length) {
            return -errno::EFAULT;
        }
        if length > scratch.len() {
            return -errno::ENAMETOOLONG;
        }
        if process.mm().copy_in(pointer, &mut scratch[..length]).is_err() {
            return -errno::EFAULT;
        }
        let Ok(text) = core::str::from_utf8(&scratch[..length]) else {
            return -errno::EINVAL;
        };
        if !block.push(text) {
            return -errno::ENAMETOOLONG;
        }
    }

    let stream = |fd: usize, read: bool| match fd {
        feos_abi::INHERIT => Some(0),
        fd => files::spawn_stream(process, fd, read),
    };
    let (Some(stdin), Some(stdout)) = (stream(stdin_fd, true), stream(stdout_fd, false)) else {
        return -errno::EBADF;
    };
    let (cwd, umask) = {
        let files = process.files();
        (load::copy(files.cwd()), files.umask)
    };
    let Some(cwd) = cwd else {
        return -errno::ENOMEM;
    };
    let Ok(start) = UserStart::new(&path, &block, &[], &cwd, umask) else {
        return -errno::ENOMEM;
    };
    // The program's stream of its own, if it inherits, is the scheduler's business.
    let raw = RawStreams { stdin, stdout, umask: Some(umask), ..RawStreams::default() };
    // A parent with as many children as it may have is as full as a table with no slot.
    if process.reserve_child().is_err() {
        return -errno::EBUSY;
    }
    match sched::spawn_program(name_of(&path), start, block, raw) {
        Ok(pid) => {
            process.child_made(pid);
            pid as isize
        }
        Err(fail) => {
            process.child_failed();
            match fail {
                SpawnFail::Spawn(SpawnError::NoFreeSlot) => -errno::EBUSY,
                SpawnFail::Spawn(SpawnError::ArgsTooLong) => -errno::ENAMETOOLONG,
                SpawnFail::Spawn(SpawnError::NoMemory) => -errno::ENOMEM,
                _ => -errno::EINVAL,
            }
        }
    }
}

/// v1's `wait`: the child's status, and its record gone with it.
pub fn sys_wait(process: &Process, pid: usize) -> isize {
    let pid = pid as u32;
    let status = sched::wait_child(pid, process.pid());
    // Reaped, or its status is gone: either way, nothing is left to wait for.
    process.child_reaped(pid);
    match status {
        Some(status) => status as isize,
        None => -errno::ECHILD,
    }
}

/// A pid argument, 32-bit: 0 or positive, or `None`.
fn pid_arg(raw: usize) -> Option<u32> {
    int_arg(raw).and_then(|pid| u32::try_from(pid).ok())
}

/// `getpgid` and `getsid`.
pub fn sys_getpgid_getsid(slot: usize, pid: usize, session: bool) -> isize {
    let Some(pid) = int_arg(pid) else {
        return -errno::EINVAL;
    };
    let Ok(pid) = u32::try_from(pid) else {
        return -errno::ESRCH;
    };
    match sched::group_of(slot, pid) {
        Ok((pgid, sid)) => (if session { sid } else { pgid }) as isize,
        Err(error) => error,
    }
}

/// `setpgid`.
pub fn sys_setpgid(slot: usize, pid: usize, pgid: usize) -> isize {
    let (Some(pid), Some(pgid)) = (pid_arg(pid), pid_arg(pgid)) else {
        return -errno::EINVAL;
    };
    match sched::set_process_group(slot, pid, pgid) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

/// `setsid`.
pub fn sys_setsid(slot: usize) -> isize {
    match sched::new_session(slot) {
        Ok(sid) => sid as isize,
        Err(error) => error,
    }
}

/// `getrusage`. CPU time is all `utime`: the scheduler doesn't split a task's time
/// between its program and its calls.
pub fn sys_getrusage(slot: usize, process: &Process, who: usize, out: usize) -> isize {
    let ns = match int_arg(who) {
        Some(rusage::SELF) => sched::process_cpu_ns(process),
        Some(rusage::THREAD) => sched::thread_cpu_ns(slot),
        // Reaped children's use is stage 1c.8's, which has exit records to keep it.
        Some(rusage::CHILDREN) => {
            return match process.mm().copy_out(out, &Rusage::default().to_bytes()) {
                Ok(()) => 0,
                Err(error) => error,
            };
        }
        _ => return -errno::EINVAL,
    };
    let (faults, peak) = process.mm().usage();
    let usage = Rusage {
        utime: Timeval::from_nanos(ns),
        maxrss: (peak * PAGE_SIZE / 1024) as i64,
        minflt: faults as i64,
        ..Rusage::default()
    };
    match process.mm().copy_out(out, &usage.to_bytes()) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

/// `sysinfo`: as much of a [`SysInfo`] as the caller has room for.
pub fn sys_sysinfo(process: &Process, out: usize, size: usize) -> isize {
    let length = size.min(SysInfo::SIZE);
    if length == 0 {
        return 0;
    }
    let cpus = sched::online_cpus() as u32;
    let free = KernelFrames.free_count().saturating_sub(PROMISES.held());
    let mut info = SysInfo {
        abi_version: feos_abi::ABI_VERSION,
        page_size: PAGE_SIZE as u32,
        cpus_online: cpus,
        cpus_configured: cpus,
        total_pages: (memory::total_bytes() / PAGE_SIZE) as u64,
        free_pages: free as u64,
        uptime_ns: time::monotonic_ns(),
        arg_max: spawn::ARG_MAX as u64,
        fd_max: MAX_FDS as u32,
        task_max: sched::task_slots() as u32,
        path_max: feos_abi::PATH_MAX as u32,
        name_max: feos_abi::NAME_MAX as u32,
        memory_limit: feos_abi::USER_MEMORY_LIMIT as u64,
        flags: if time::unix_time().is_none() { sysinfo::NO_RTC } else { 0 },
        features: feos_abi::FEATURES,
        ..SysInfo::default()
    };
    let release = feos_app::VERSION.as_bytes();
    let count = release.len().min(info.release.len() - 1);
    info.release[..count].copy_from_slice(&release[..count]);
    info.machine[..load::PLATFORM.len()].copy_from_slice(load::PLATFORM.as_bytes());
    match process.mm().copy_out(out, &info.to_bytes()[..length]) {
        Ok(()) => 0,
        Err(error) => error,
    }
}
