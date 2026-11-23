//! The process calls of stages 1c.6 to 1c.8: `SPAWN2` (and v1's `SPAWN` beside it),
//! `EXEC`, `WAIT4` (and v1's `WAIT` on top of it), `KILL`, process groups and sessions,
//! `GETRUSAGE` and `SYSINFO`.
//!
//! `SPAWN2` does in the call everything that can fail, before the child exists, so every
//! failure is the call's result and none a child's exit status (section 3.4 of the ABI
//! design): the request and its arrays are copied once (the strings into staging frames,
//! see [`load::Staged`]), the child's descriptor table is built from a copy of the
//! caller's by the actions, its program is resolved and held, and its exit record taken
//! in the caller. Then the task is made, and builds its address space itself.
//!
//! `EXEC` does the same checks, and builds the new image too, in a space of its own, before
//! its point of no return: every way it can fail is the call's error. Past that it can't
//! fail. Holding the creation lock, it has every other thread end and waits until each
//! has, then puts the new space and program in place of the old ones, closes the
//! close-on-exec descriptors, and goes to the new program in the same thread.
//!
//! `WAIT4` looks through the caller's exit records ([`super::children`]), which its
//! children's ends, stops and continues fill in under the scheduler's lock, and waits for
//! the counter every one of those moves on. `KILL` carries out a signal's default action
//! (stage 1c.8 has no handlers): the scheduler ends, stops or continues the target.
use alloc::string::String;
use alloc::vec::Vec;

use feos_abi::{at, errno, int_arg, o, rusage, spawn, spawn_action as kind, sysinfo, wait, ExecRequest, Rusage, SpawnAction, SpawnRequest, SysInfo, Timeval, UserStr, MAX_FDS, PAGE_SIZE};
use feos_signal::Status;
use feos_app::{ArgBlock, SpawnError, PROCESS_ARGS_MAX};
use feos_fs::vfs::{path, ACCESS_EXECUTE};
use feos_fs::FileKind;
use feos_vm::FrameSource;

use super::children::Usage;
use super::files::{self, Files, Object, OfdRef, Request};
use super::fs_calls::{self, errno_of};
use super::load::{self, Staged, UserStart};
use super::memory_calls::{KernelFrames, PROMISES};
use super::process::{HeapCharge, Process, UserMm};
use super::{tree, KILLED};
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
fn stage_strings(process: &Process, (argv, argc): (u64, u64), (envp, envc): (u64, u64)) -> Result<Staged, isize> {
    let user = process.mm();
    let (argc, envc) = (argc as usize, envc as usize);
    let size = core::mem::size_of::<UserStr>();
    let mut table = Staged::new((argc + envc) * size, Some(process))?;
    table.append_user(user, argv as usize, argc * size)?;
    table.append_user(user, envp as usize, envc * size)?;
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

/// A spawn's actions, and the heap they take charged to its caller for as long as
/// they're kept.
struct Actions {
    list: Vec<SpawnAction>,
    _charge: Option<HeapCharge>,
}

/// The actions, copied once, each checked: a kind there is, and nothing in the fields it
/// doesn't use.
fn copy_actions(process: &Process, request: &SpawnRequest) -> Result<Actions, isize> {
    let count = request.action_count as usize;
    let mut actions = Vec::new();
    if count == 0 {
        return Ok(Actions { list: actions, _charge: None });
    }
    let charge = process.charge_heap(count * (SpawnAction::SIZE + core::mem::size_of::<SpawnAction>()))?;
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
    Ok(Actions { list: actions, _charge: Some(charge) })
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
///
/// `TCSETPGRP` is checked here, in its turn, and leaves its terminal in `foreground`, for
/// the scheduler to make the child's group its foreground one as the child is made: the
/// group doesn't exist before then.
fn run_action(
    slot: usize,
    process: &Process,
    files: &mut Files,
    action: &SpawnAction,
    closed: &mut Vec<OfdRef>,
    foreground: &mut Option<super::tty::TerminalRef>,
) -> Result<(), isize> {
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
        kind::TCSETPGRP => {
            let fd = action_fd(action.fd)?;
            let ofd = files.table.get(fd).ok_or(-errno::EBADF)?;
            let Object::Terminal { terminal, .. } = &ofd.object else {
                return Err(-errno::ENOTTY);
            };
            // As `TTY_SET_PGRP`: only a terminal of the caller's own session.
            let (_, sid) = sched::group_of(slot, 0)?;
            if terminal.session() != sid {
                return Err(-errno::ENOTTY);
            }
            *foreground = Some(terminal.clone());
            Ok(())
        }
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
    let mut strings = stage_strings(process, (request.argv, request.argc), (request.envp, request.envc))?;
    // The staged frames count against this process's limit until the call returns, by
    // when the child has them.
    let _pages = strings.take_pages();
    let actions = copy_actions(process, &request)?;

    // The child's table: the caller's, then the actions in order, then whatever is to
    // close on exec closed. What closes is let go here, with no table held.
    let mut files = process.files().try_clone()?;
    let mut closed = Vec::new();
    let mut foreground = None;
    for action in &actions.list {
        run_action(slot, process, &mut files, action, &mut closed, &mut foreground)?;
    }
    // A new session isn't the terminal's: its group can't be the foreground one.
    if foreground.is_some() && request.flags & spawn::SETSID != 0 {
        return Err(-errno::EPERM);
    }
    drop(closed);
    drop(files.table.cloexec_sweep());

    // The program, against the child's table and working directory: by its path, or
    // the file a descriptor there holds, by identity, whatever its path names now.
    let (uid, gid) = sched::credentials(slot);
    let (program, mut image) = resolve_target(&target(&files, request.path.dirfd, given.as_deref())?, uid, gid)?;
    // A script's interpreter runs with its strings in front of the rest.
    let _script_pages = match image.heads.is_empty() {
        true => None,
        false => {
            strings = strings.with_heads(&core::mem::take(&mut image.heads), Some(process))?;
            strings.take_pages()
        }
    };
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
    // Handlers aren't kept by a new image; what's ignored is, but for what `SETSIGDEF`
    // names, which starts at its default.
    let sigmask = (request.flags & spawn::SETSIGMASK != 0).then_some(request.sigmask & !UNBLOCKABLE);
    let sigdefault = if request.flags & spawn::SETSIGDEF != 0 { request.sigdefault } else { 0 };
    // The terminal's word is in its static table, and `foreground` holds the terminal
    // until the child, made, has set it.
    let raw = RawStreams {
        stdin: 0,
        stdout: 0,
        umask: Some(files.umask),
        group,
        sigmask,
        sigdefault,
        foreground: foreground.as_ref().map(super::tty::TerminalRef::foreground_word),
    };
    let shown = strings.shown();
    let given = given.unwrap_or_default();
    let cwd = load::copy(files.cwd()).ok_or(-errno::ENOMEM)?;
    let umask = files.umask;

    let record = process.reserve_child()?;
    let Some(start) = UserStart::make(&program, Some(image), if given.is_empty() { &program } else { &given }, &cwd, umask, strings, Some(files)) else {
        process.child_failed(&record);
        return Err(-errno::ENOMEM);
    };
    match sched::spawn_program(&name, start, shown, raw, record.clone()) {
        Ok(pid) => Ok(pid),
        Err(fail) => {
            process.child_failed(&record);
            Err(spawn_errno(fail))
        }
    }
}

/// What a spawn or exec is asked to run, found in a descriptor table: the absolute path
/// asked for, or with `SPAWN_EMPTY_PATH` the description whose file it is.
enum Target {
    Path(String),
    Held(OfdRef),
}

/// The program a spawn or exec runs, against `files` (the child's table, or the caller's
/// own for `EXEC`): `given` joined to `dirfd` or the working directory there, or with
/// `SPAWN_EMPTY_PATH` (`given` `None`) the description of the descriptor `dirfd`.
fn target(files: &Files, dirfd: i32, given: Option<&str>) -> Result<Target, isize> {
    match given {
        Some(given) => Ok(Target::Path(join_child(files, dirfd, given)?)),
        None => {
            let ofd = usize::try_from(dirfd).ok().and_then(|fd| files.table.get(fd)).ok_or(-errno::EBADF)?;
            Ok(Target::Held(ofd.clone()))
        }
    }
}

/// The program `target` names, resolved, checked and held, and the absolute path it was
/// asked by: for a held one, the file it holds by identity, whatever its path names now.
fn resolve_target(target: &Target, uid: u32, gid: u32) -> Result<(String, load::Image), isize> {
    let tree = tree().ok_or(-errno::EIO)?;
    match target {
        Target::Path(program) => {
            let image = load::resolve(tree, program, uid, gid).map_err(|refused| refused.errno)?;
            Ok((load::copy(program).ok_or(-errno::ENOMEM)?, image))
        }
        Target::Held(ofd) => {
            let Object::File { path, id, entry: Some(entry), .. } = &ofd.object else {
                return Err(-errno::EACCES);
            };
            let image = load::resolve_held(tree, path, *id, entry, uid, gid).map_err(|refused| refused.errno)?;
            Ok((load::copy(path).ok_or(-errno::ENOMEM)?, image))
        }
    }
}

/// `exec`: only returns with an error, from before its point of no return.
pub fn sys_exec(slot: usize, process: &Process, address: usize) -> isize {
    match exec(slot, process, address) {
        Err(error) => error,
        Ok(never) => match never {},
    }
}

fn exec(slot: usize, process: &Process, address: usize) -> Result<core::convert::Infallible, isize> {
    let mut bytes = [0u8; ExecRequest::SIZE];
    process.mm().copy_in(address, &mut bytes)?;
    let request = ExecRequest::from_bytes(&bytes);
    let flags = request.flags;
    if request.reserved != [0; 2]
        || request.path.reserved != 0
        || flags & !spawn::EMPTY_PATH != 0
        || (flags & spawn::EMPTY_PATH != 0 && request.path.length != 0)
        || request.argc == 0
    {
        return Err(-errno::EINVAL);
    }
    if request.argc.checked_add(request.envc).is_none_or(|count| count > spawn::MAX_STRINGS as u64) {
        return Err(-errno::E2BIG);
    }
    let given = match flags & spawn::EMPTY_PATH {
        0 => Some(fs_calls::user_path(process, request.path.pointer as usize, request.path.length as usize)?),
        _ => None,
    };
    let mut strings = stage_strings(process, (request.argv, request.argc), (request.envp, request.envc))?;
    let pages = strings.take_pages();

    // The program, against this process's own table and working directory, as `SPAWN2`
    // resolves its child's. The table isn't held while it's looked for.
    let (uid, gid) = sched::credentials(slot);
    let target = target(&process.files(), request.path.dirfd, given.as_deref())?;
    let (program, mut image) = resolve_target(&target, uid, gid)?;
    drop(target);
    let script_pages = match image.heads.is_empty() {
        true => None,
        false => {
            strings = strings.with_heads(&core::mem::take(&mut image.heads), Some(process))?;
            strings.take_pages()
        }
    };
    let shown = strings.shown();
    let name = load::copy(name_of(&image.canonical)).ok_or(-errno::ENOMEM)?;
    let exe = load::copy(&image.canonical).ok_or(-errno::ENOMEM)?;

    // The new image, whole, in a space nobody runs on yet: everything that can fail
    // fails here, with the process as it was.
    let tree = tree().ok_or(-errno::EIO)?;
    let space = UserMm::new(true).ok_or(-errno::ENOMEM)?;
    let shown_as = given.as_deref().unwrap_or(&program);
    let frame = load::build_image(slot, &space, tree, &image, &strings, shown_as).map_err(|refused| refused.errno)?;
    drop((strings, image, pages, script_pages, given, program));

    // The point of no return. Alone, under the creation lock, so no thread can start.
    let Some(creating) = process.creating() else {
        return Err(KILLED);
    };
    if !sched::end_other_threads(slot) {
        return Err(KILLED);
    }
    sched::switch_space(slot, space.root(), space.tlb());
    let (root, tlb) = (space.root(), core::ptr::from_ref(space.tlb()));
    // SAFETY: the only thread, holding the creation lock; nothing above holds a reference
    // from `mm()` or `exe()` past here (`dispatch`'s own is never used again: this call
    // doesn't return).
    let (old, old_exe) = unsafe { process.replace_image(space, exe) };
    sched::exec_done(slot, &name, shown);
    // Handlers belong to the old image's code.
    process.signals().exec_reset();
    drop(creating);
    // What was open close-on-exec closes now, each with the record locks the process held
    // on its file, one at a time, so nothing is allocated past the point of no return.
    let mut fd = 0;
    while fd < MAX_FDS {
        let closed = {
            let mut files = process.files();
            match files.table.cloexec(fd) {
                Some(true) => files.table.remove(fd).ok(),
                _ => None,
            }
        };
        if let Some(ofd) = closed {
            super::lock_calls::descriptor_closed(process, &ofd);
            drop(ofd);
        }
        fd += 1;
    }
    // The old space, in task context, its surface with it; and anything it, or a closed
    // descriptor, held last that was unlinked while in use.
    drop(old);
    drop(old_exe);
    super::memory_calls::forget_fault(slot);
    fs_calls::sweep_orphans();
    // SAFETY: the space is the process's now, and lives as long as it does.
    sched::enter_user(slot, root, unsafe { &*tlb }, frame)
}

/// What a forked child starts from, made by its parent's `FORK`: its space, copied on
/// write; its descriptor table, sharing the parent's open file descriptions; its program
/// path and dispositions; and the calling thread's registers (the call's result 0 in
/// them), thread pointer and alternate stack.
pub struct Forked {
    mm: alloc::sync::Arc<UserMm>,
    files: Files,
    exe: String,
    signals: super::signal::Signals,
    frame: crate::Frame,
    tls: usize,
    alt_stack: (u64, u64),
}

/// `fork`.
pub fn sys_fork(slot: usize, process: &Process) -> isize {
    // What the child is made in comes off the heap before any lock is held: a heap that's
    // run out panics, which with a lock held would stop the machine rather than end this.
    let cell = alloc::sync::Arc::<UserMm>::new_uninit();
    let boxed = alloc::boxed::Box::<Forked>::new_uninit();
    // Held throughout, so no thread of this process is half made in the copy, nor an
    // `EXEC` under way: the copy is of the calling thread and a whole process.
    let Some(_creating) = process.creating() else {
        return KILLED;
    };
    let record = match process.reserve_child() {
        Ok(record) => record,
        Err(error) => return error,
    };
    match fork(slot, process, cell, boxed, &record) {
        Ok(pid) => pid as isize,
        Err(error) => {
            process.child_failed(&record);
            error
        }
    }
}

fn fork(
    slot: usize,
    process: &Process,
    cell: alloc::sync::Arc<core::mem::MaybeUninit<UserMm>>,
    boxed: alloc::boxed::Box<core::mem::MaybeUninit<Forked>>,
    record: &feos_sync::TryArc<super::ChildRecord>,
) -> Result<u32, isize> {
    // The table, sharing every description (and so offsets and `flock` locks); record
    // locks are the process's, by pid, so the child has none.
    let files = process.files().try_clone()?;
    let umask = files.umask;
    let exe = load::copy(process.exe()).ok_or(-errno::ENOMEM)?;
    let signals = process.signals().fork_copy();
    // The space last: what it promises is held from here, and given back with it.
    let mm = process.mm().fork(cell)?;
    // SAFETY: the calling task is in this call, so nothing else is using its user frame.
    let mut frame = unsafe { *sched::user_frame(slot) };
    feos_hal::TrapContext::set_syscall_result(&mut frame, 0);
    let forked = alloc::boxed::Box::write(
        boxed,
        Forked { mm, files, exe, signals, frame, tls: sched::current_tls(slot), alt_stack: sched::alt_stack(slot) },
    );
    // Its group, session and mask are the caller's; its streams' pipes are held by it too.
    let raw = RawStreams { umask: Some(umask), ..RawStreams::default() };
    sched::spawn_fork(forked, raw, record.clone()).map_err(spawn_errno)
}

/// A forked child's first steps, on its own task: its process from what its parent
/// copied, its thread pointer and alternate stack as the forking thread had them, and
/// then its program, where the parent was, with `FORK` returning 0. Only returns if
/// there's no memory for the process, with the status it ends with.
pub fn start_forked(slot: usize, forked: alloc::boxed::Box<Forked>) -> i32 {
    let Forked { mm, files, exe, signals, frame, tls, alt_stack } = *forked;
    let Some(process) = super::process::create_forked(slot, super::current_pid(), mm, files, exe, signals) else {
        super::say(format_args!("fork: out of memory"));
        return feos_abi::EXIT_NOT_EXECUTABLE;
    };
    sched::set_tls(slot, tls);
    sched::set_alt_stack(slot, alt_stack.0, alt_stack.1);
    sched::enter_user(slot, process.mm().root(), process.mm().tlb(), frame)
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
    let Ok(mut start) = UserStart::new(&path, &block, &[], &cwd, umask) else {
        return -errno::ENOMEM;
    };
    // A stream from a terminal-marked descriptor (an inherited one is this process's
    // own, 0 or 1; stderr always is its 2) is on that terminal in the child too.
    let sources = [
        if stdin_fd == feos_abi::INHERIT { 0 } else { stdin_fd },
        if stdout_fd == feos_abi::INHERIT { 1 } else { stdout_fd },
        2,
    ];
    let mut terminal: Option<super::tty::TerminalRef> = None;
    let mut streams = 0u8;
    for (stream, fd) in sources.into_iter().enumerate() {
        if let Some(found) = files::terminal_of_fd(process, fd) {
            if terminal.as_ref().is_none_or(|terminal| terminal.same(&found)) {
                streams |= 1 << stream;
                terminal.get_or_insert(found);
            }
        }
    }
    start.set_terminal(terminal, streams);
    // The program's stream of its own, if it inherits, is the scheduler's business.
    let raw = RawStreams { stdin, stdout, umask: Some(umask), ..RawStreams::default() };
    // A parent with as many children as it may have is as full as a table with no slot.
    let Ok(record) = process.reserve_child() else {
        return -errno::EBUSY;
    };
    match sched::spawn_program(name_of(&path), start, block, raw, record.clone()) {
        Ok(pid) => pid as isize,
        Err(fail) => {
            process.child_failed(&record);
            match fail {
                SpawnFail::Spawn(SpawnError::NoFreeSlot) => -errno::EBUSY,
                SpawnFail::Spawn(SpawnError::ArgsTooLong) => -errno::ENAMETOOLONG,
                SpawnFail::Spawn(SpawnError::NoMemory) => -errno::ENOMEM,
                _ => -errno::EINVAL,
            }
        }
    }
}

/// v1's `wait`: `wait4(pid, ..., 0, 0)` for one child, returning v1's status, the exit
/// code or 128 plus the signal. Only a pid a child can have: 0 is a record still being
/// made, and a pid with its upper bits set would be cut to one that may be another
/// child's. A child reaped already is answered again from the ring of recent exits, as
/// v1 always was, while the ring still has it.
pub fn sys_wait(slot: usize, process: &Process, pid: usize) -> isize {
    let Some(pid) = u32::try_from(pid).ok().filter(|&pid| pid != 0 && pid <= i32::MAX as u32) else {
        return -errno::ECHILD;
    };
    match wait_child(slot, process, Wanted::Pid(pid), 0) {
        Ok(Some(found)) => found.status.v1() as isize,
        Ok(None) => -errno::ECHILD,
        Err(error) if error == -errno::ECHILD => sched::recent_exit(pid, process.pid()).map_or(error, |status| status as isize),
        Err(error) => error,
    }
}

/// Which children a wait is for.
#[derive(Clone, Copy)]
enum Wanted {
    Pid(u32),
    Group(u32),
    Any,
}

/// A child a wait found: its pid and what it's reported with, and, if it was reaped,
/// what it used.
struct Found {
    pid: u32,
    status: Status,
    usage: Option<Usage>,
}

/// What a look through the records came to.
enum Look {
    /// No record matches: `ECHILD`.
    None,
    /// Some do, and nothing has happened to any that's wanted, or (`true`) one has ended
    /// and is still giving back what it held, which takes no longer than that: even
    /// `WNOHANG` waits for it, so a `SIGCHLD` handler's `WAIT4` always reaps what it was
    /// told of.
    Waiting(bool),
    Found(Found),
}

/// Looks once through `process`'s records for a child `wanted` names whose end (or, as
/// `options` ask, stop or continue) is there to report: an end reaps the record.
fn look(process: &Process, wanted: Wanted, options: usize) -> Look {
    let mut matched = false;
    let mut settling = false;
    let mut detached = Vec::new();
    let reaped = {
        let mut children = process.children();
        // Children nobody waits for (`SIGCHLD` ignored, or `SA_NOCLDWAIT`, as they ended)
        // go once they've settled, matching nothing: a wait for them is `ECHILD`. Taken
        // out under the lock and dropped after, a few at a time, as nothing is allocated
        // for more.
        let mut index = 0;
        while index < children.len() {
            let record = &children[index];
            if record.detached() && record.end().is_some() && sched::settled(record.pid()) && detached.try_reserve(1).is_ok() {
                detached.push(children.swap_remove(index));
            } else {
                index += 1;
            }
        }
        let mut found = None;
        for (index, record) in children.iter().enumerate() {
            let pid = record.pid();
            let named = match wanted {
                Wanted::Pid(wanted) => pid == wanted,
                Wanted::Group(pgid) => record.pgid() == pgid,
                Wanted::Any => true,
            };
            // A record with no pid yet is a child some thread is still making.
            if pid == 0 || !named {
                continue;
            }
            matched = true;
            if let Some(status) = record.end() {
                // Reaped only once every thread of it has given back what it held.
                if sched::settled(pid) {
                    found = Some((index, Found { pid, status, usage: Some(record.usage()) }));
                    break;
                }
                settling = true;
                continue;
            }
            let taken = record.take_change(|status| match status {
                Status::Stopped(_) => options & wait::UNTRACED != 0,
                Status::Continued => options & wait::CONTINUED != 0,
                _ => false,
            });
            if let Some(status) = taken {
                return Look::Found(Found { pid, status, usage: None });
            }
        }
        // The record goes once the list's lock has: its pid may be given again then.
        found.map(|(index, found)| (children.swap_remove(index), found))
    };
    match reaped {
        Some((record, found)) => {
            drop(record);
            if let Some(usage) = found.usage {
                process.add_reaped(usage);
            }
            Look::Found(found)
        }
        None if matched => Look::Waiting(settling),
        None => Look::None,
    }
}

/// Waits for a child `wanted` names to end (or, as `options` ask, to stop or continue),
/// as `WAIT4` does. `None` if nothing matches, or, with `WNOHANG`, nothing has happened.
fn wait_child(slot: usize, process: &Process, wanted: Wanted, options: usize) -> Result<Option<Found>, isize> {
    // The caller's own group is looked up each time round: it may change meanwhile.
    loop {
        let seen = sched::exit_sequence();
        let wanted = match wanted {
            Wanted::Group(0) => Wanted::Group(sched::group_of(slot, 0)?.0),
            other => other,
        };
        let settling = match look(process, wanted, options) {
            Look::Found(found) => return Ok(Some(found)),
            Look::None => return Err(-errno::ECHILD),
            Look::Waiting(false) if options & wait::NOHANG != 0 => return Ok(None),
            Look::Waiting(settling) => settling,
        };
        // A kill gives up the wait, and the call unwinds; so does a signal to take, but
        // not the brief wait of a `WNOHANG` for a child's teardown, which isn't a wait
        // for the child at all.
        let by_signal = !(settling && options & wait::NOHANG != 0);
        if !sched::wait_for_exits(seen, by_signal) {
            return Err(if sched::kill_requested() { KILLED } else { sched::interrupted_result() });
        }
    }
}

/// `wait4`. The status and resource-use pointers are checked before anything is reaped;
/// one another thread unmaps after that loses the report (`EFAULT`), as on Linux.
pub fn sys_wait4(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [pid, status_out, options, rusage_out, ..] = args;
    let Some(pid) = int_arg(pid) else {
        return -errno::EINVAL;
    };
    if options & !wait::ALL != 0 {
        return -errno::EINVAL;
    }
    let wanted = match pid {
        -1 => Wanted::Any,
        0 => Wanted::Group(0),
        pid if pid > 0 => Wanted::Pid(pid as u32),
        // No group has the pid `i32::MIN` would name.
        pid => match pid.checked_neg() {
            Some(pgid) => Wanted::Group(pgid as u32),
            None => return -errno::ECHILD,
        },
    };
    for (pointer, length) in [(status_out, 4), (rusage_out, Rusage::SIZE)] {
        if pointer != 0 {
            if let Err(error) = process.mm().fault_in(pointer, length, true) {
                return error;
            }
        }
    }
    let found = match wait_child(slot, process, wanted, options) {
        Ok(Some(found)) => found,
        Ok(None) => return 0,
        Err(error) => return error,
    };
    if status_out != 0 {
        if let Err(error) = process.mm().copy_out(status_out, &found.status.wait_status().to_ne_bytes()) {
            return error;
        }
    }
    if rusage_out != 0 {
        let usage = found.usage.map_or(Rusage::default(), rusage_of);
        if let Err(error) = process.mm().copy_out(rusage_out, &usage.to_bytes()) {
            return error;
        }
    }
    found.pid as isize
}

/// A [`Rusage`] of what reaped children used.
fn rusage_of(usage: Usage) -> Rusage {
    Rusage {
        utime: Timeval::from_nanos(sched::ticks_ns(usage.ticks)),
        maxrss: (usage.peak as usize * PAGE_SIZE / 1024) as i64,
        minflt: usage.faults as i64,
        ..Rusage::default()
    }
}

/// `kill`.
pub fn sys_kill(slot: usize, process: &Process, pid: usize, signal: usize) -> isize {
    let (Some(pid), Some(signal)) = (int_arg(pid), int_arg(signal)) else {
        return -errno::EINVAL;
    };
    let Some(signal) = u32::try_from(signal).ok().filter(|&signal| signal == 0 || feos_signal::is_signal(signal)) else {
        return -errno::EINVAL;
    };
    let target = match pid {
        -1 => sched::KillTarget::All,
        0 => match sched::group_of(slot, 0) {
            Ok((pgid, _)) => sched::KillTarget::Group(pgid),
            Err(error) => return error,
        },
        pid if pid > 0 => sched::KillTarget::Process(pid as u32),
        pid => match pid.checked_neg() {
            Some(pgid) => sched::KillTarget::Group(pgid as u32),
            None => return -errno::ESRCH,
        },
    };
    match sched::kill_from(slot, target, signal) {
        Ok(()) => 0,
        // A child that has ended and isn't reaped yet still exists, and a signal to it
        // does nothing, as for a POSIX zombie.
        Err(error) if error == -errno::ESRCH && is_unreaped_child(process, pid) => 0,
        Err(error) => error,
    }
}

/// Whether `pid` is one of `process`'s children, ended or not, that it hasn't reaped.
fn is_unreaped_child(process: &Process, pid: i32) -> bool {
    pid > 0 && process.children().iter().any(|record| record.pid() == pid as u32)
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
        // Reaped children's, and theirs, from their records.
        Some(rusage::CHILDREN) => {
            return match process.mm().copy_out(out, &rusage_of(process.reaped()).to_bytes()) {
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
        flags: if time::have_rtc() { 0 } else { sysinfo::NO_RTC }
            | if crate::random::seeded() { sysinfo::RNG_SEEDED } else { 0 }
            | if crate::random::trusted() { sysinfo::RNG_TRUSTED } else { 0 },
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
