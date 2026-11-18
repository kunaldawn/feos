//! Starting a program: what it starts with, staged before its task exists, and building
//! its address space and its first stack, in its own task.
//!
//! A program's argument and environment strings are copied once, by whoever starts it,
//! into frames of their own ([`Staged`]): `SPAWN2` copies them from its caller's memory
//! in the call, so nothing the caller does afterwards can change them, and a kernel app's
//! from its own. The frames are taken through the machine's promises, never from the
//! kernel heap, so many programs starting at once with a megabyte of arguments each meet
//! `ENOMEM`, not a heap that's run out. The new task copies them to the top of its
//! argument area as the System V initial stack (section 7.1 of the ABI design) and lets
//! them go.
//!
//! What it runs is resolved either in the call (`SPAWN2`: every way it can fail is the
//! call's error, and the file is held from then on, [`Image`]) or by the new task itself
//! (v1's `SPAWN` and kernel apps: a program that isn't there ends with 127, one that
//! can't be run with 126, as v1 always did).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use feos_abi::{aux, errno, spawn, UserStr};
use feos_app::{ArgBlock, SpawnError};
use feos_elf::Limits;
use feos_fs::vfs::{path, FileId, VfsTree, ACCESS_EXECUTE};
use feos_fs::{FileKind, FsError};
use feos_hal::Arch;
use feos_vm::{FileMap, FileRef, FrameSource, MapRequest, Place, Prot, PAGE_SIZE};

use super::files::Files;
use super::memory_calls::{KernelFrames, PROMISES};
use super::process::{self, HeapCharge, Process, UserMm};
use super::{say, tree, NoKill};
use crate::{memory, pagecache, sched, Frame, Port};

#[cfg(target_arch = "aarch64")]
const MACHINE: u16 = feos_elf::EM_AARCH64;
#[cfg(target_arch = "x86_64")]
const MACHINE: u16 = feos_elf::EM_X86_64;

/// What `AT_PLATFORM` names.
#[cfg(target_arch = "aarch64")]
pub const PLATFORM: &str = "aarch64";
#[cfg(target_arch = "x86_64")]
pub const PLATFORM: &str = "x86_64";

/// The largest signal frame either port will build (stage 1c.9), for `AT_MINSIGSTKSZ`:
/// within `MINSIGSTKSZ`.
const SIGNAL_FRAME_MAX: u64 = 2048;

/// Entries of the aux vector, `AT_NULL` included.
const AUX_ENTRIES: usize = 24;

// The most a start can put in the argument area: the strings and their pointers, within
// `ARG_MAX` together; a `UserStr` for each argument; the aux vector; the program's two
// paths, `AT_PLATFORM`, the random bytes and alignment. All of it fits.
const _: () = assert!(
    spawn::ARG_MAX
        + spawn::MAX_STRINGS * core::mem::size_of::<UserStr>()
        + 3 * 8
        + AUX_ENTRIES * 16
        + 2 * (feos_abi::PATH_MAX + 1)
        + 16
        + 16
        + 4 * 16
        <= feos_abi::USER_ARGS_SIZE
);

/// Bytes in frames of their own, end to end, taken through the promises: a program's
/// strings, each followed by a NUL, its arguments first and then its environment, as its
/// starter copied them.
pub struct Staged {
    frames: Vec<usize>,
    /// Bytes written so far.
    length: usize,
    /// Strings among them that are arguments, and that are the environment.
    argc: usize,
    envc: usize,
    /// The heap `frames` takes, charged to the process that's starting it.
    _charge: Option<HeapCharge>,
}

impl Staged {
    /// Room for `capacity` bytes, the frames taken now. `ENOMEM` if they can't be had or
    /// promised, or the list of them charged to `charger`.
    pub fn new(capacity: usize, charger: Option<&Process>) -> Result<Self, isize> {
        let count = capacity.div_ceil(PAGE_SIZE).max(1);
        let bytes = count * core::mem::size_of::<usize>();
        let charge = match charger {
            Some(process) => Some(process.charge_heap(bytes)?),
            None => None,
        };
        let mut frames = Vec::new();
        frames.try_reserve_exact(count).map_err(|_| -errno::ENOMEM)?;
        let mut staged = Self { frames, length: 0, argc: 0, envc: 0, _charge: charge };
        // Promised first, as a mapping's pages are, so these can't take frames other
        // programs were promised; given back once each is had.
        if !PROMISES.promise(count, KernelFrames.free_count()) {
            return Err(-errno::ENOMEM);
        }
        for _ in 0..count {
            let _no_kill = NoKill::new();
            match memory::alloc_frames(1, 1) {
                // Room was reserved for every one.
                Some(frame) => staged.frames.push(frame),
                None => break,
            }
        }
        PROMISES.release(count);
        if staged.frames.len() < count {
            return Err(-errno::ENOMEM);
        }
        Ok(staged)
    }

    /// The part of the frames from byte `at` to the end of its frame.
    fn room_at(&mut self, at: usize) -> Option<&mut [u8]> {
        let frame = *self.frames.get(at / PAGE_SIZE)?;
        let offset = at % PAGE_SIZE;
        // SAFETY: a frame of this staging's own, which the identity map covers, from
        // `offset` to its end, borrowed for as long as `self` is.
        Some(unsafe { core::slice::from_raw_parts_mut((frame + offset) as *mut u8, PAGE_SIZE - offset) })
    }

    /// The part of the frames from byte `at` to the end of its frame, to read.
    fn bytes_at(&self, at: usize) -> Option<&[u8]> {
        let frame = *self.frames.get(at / PAGE_SIZE)?;
        let offset = at % PAGE_SIZE;
        // SAFETY: as in `room_at`, read only.
        Some(unsafe { core::slice::from_raw_parts((frame + offset) as *const u8, PAGE_SIZE - offset) })
    }

    /// Adds a string from the kernel and its NUL. `false` if there's no room.
    fn push(&mut self, text: &[u8], env: bool) -> bool {
        let mut done = 0;
        while done < text.len() {
            let at = self.length + done;
            let Some(room) = self.room_at(at) else {
                return false;
            };
            let count = room.len().min(text.len() - done);
            room[..count].copy_from_slice(&text[done..done + count]);
            done += count;
        }
        self.finish_string(text.len(), env)
    }

    /// Adds a string of `length` bytes from a program's memory at `pointer`, and its NUL:
    /// `EFAULT` if it can't be read, `EINVAL` if it has a NUL in it.
    pub fn push_user(&mut self, user: &UserMm, pointer: usize, length: usize, env: bool) -> Result<(), isize> {
        if length != 0 && !user.in_window(pointer, length) {
            return Err(-errno::EFAULT);
        }
        let mut done = 0;
        while done < length {
            let at = self.length + done;
            let room = self.room_at(at).ok_or(-errno::E2BIG)?;
            let count = room.len().min(length - done);
            let piece = &mut room[..count];
            user.copy_in(pointer + done, piece)?;
            if piece.contains(&0) {
                return Err(-errno::EINVAL);
            }
            done += count;
        }
        if self.finish_string(length, env) {
            Ok(())
        } else {
            Err(-errno::E2BIG)
        }
    }

    /// Ends the string of `length` bytes just written with its NUL, and counts it.
    fn finish_string(&mut self, length: usize, env: bool) -> bool {
        let end = self.length + length;
        let Some(room) = self.room_at(end) else {
            return false;
        };
        room[0] = 0;
        self.length = end + 1;
        if env {
            self.envc += 1;
        } else {
            self.argc += 1;
        }
        true
    }

    /// Calls `each` with every string's offset and length, in order, until it fails.
    fn each_string(&self, mut each: impl FnMut(usize, usize) -> Result<(), isize>) -> Result<(), isize> {
        let mut start = 0;
        let mut at = 0;
        while at < self.length {
            let Some(bytes) = self.bytes_at(at) else {
                break;
            };
            let bytes = &bytes[..bytes.len().min(self.length - at)];
            for (index, &byte) in bytes.iter().enumerate() {
                if byte == 0 {
                    each(start, at + index - start)?;
                    start = at + index + 1;
                }
            }
            at += bytes.len();
        }
        Ok(())
    }

    /// Copies every byte to the program's memory at `address`.
    fn copy_out(&self, user: &UserMm, address: usize) -> Result<(), isize> {
        let mut at = 0;
        while at < self.length {
            let bytes = self.bytes_at(at).ok_or(-errno::EFAULT)?;
            let bytes = &bytes[..bytes.len().min(self.length - at)];
            user.copy_out(address + at, bytes)?;
            at += bytes.len();
        }
        Ok(())
    }

    /// Appends `length` bytes from a program's memory at `pointer` as they are: an array
    /// a call copies once, to read from here after.
    pub fn append_user(&mut self, user: &UserMm, pointer: usize, length: usize) -> Result<(), isize> {
        if length != 0 && !user.in_window(pointer, length) {
            return Err(-errno::EFAULT);
        }
        let mut done = 0;
        while done < length {
            let at = self.length + done;
            let room = self.room_at(at).ok_or(-errno::EFAULT)?;
            let count = room.len().min(length - done);
            user.copy_in(pointer + done, &mut room[..count])?;
            done += count;
        }
        self.length += length;
        Ok(())
    }

    /// The `u64` at byte `at`, which [`append_user`](Self::append_user) wrote: aligned, so
    /// within one frame.
    pub fn word(&self, at: usize) -> u64 {
        let mut out = [0u8; 8];
        if let Some(bytes) = self.bytes_at(at) {
            if bytes.len() >= 8 && at + 8 <= self.length {
                out.copy_from_slice(&bytes[..8]);
            }
        }
        u64::from_ne_bytes(out)
    }

    /// As many of the arguments as fit the process table's block, for `ps` to show.
    pub fn shown(&self) -> ArgBlock {
        let mut block = ArgBlock::new();
        let mut index = 0;
        let mut full = false;
        let mut piece = [0u8; 256];
        let _ = self.each_string(|offset, length| {
            if full || index >= self.argc {
                return Ok(());
            }
            index += 1;
            let length = length.min(piece.len());
            let mut done = 0;
            while done < length {
                let Some(bytes) = self.bytes_at(offset + done) else {
                    break;
                };
                let count = bytes.len().min(length - done);
                piece[done..done + count].copy_from_slice(&bytes[..count]);
                done += count;
            }
            let text = core::str::from_utf8(&piece[..done]).unwrap_or("?");
            full = !block.push(text);
            Ok(())
        });
        block
    }

}

impl Drop for Staged {
    fn drop(&mut self) {
        for &frame in &self.frames {
            memory::free_frames(frame, 1);
        }
    }
}

/// A program resolved, checked and held: what `SPAWN2` found in the call, for its child to
/// load. The cache entry holds the file by identity, so a rename or unlink in between
/// changes nothing about what runs.
pub struct Image {
    /// The canonical path, for `CURRENT_EXE` and `AT_FEOS_EXE`.
    pub canonical: String,
    pub id: FileId,
    pub size: u64,
    /// The file's hold, if the cache keeps it.
    _hold: Option<FileRef>,
}

/// Why a program can't run: the error a call gives, and what v1 prints.
pub struct Refused {
    pub errno: isize,
    pub message: &'static str,
}

impl Refused {
    const fn new(errno: isize, message: &'static str) -> Self {
        Self { errno: -errno, message }
    }

    /// The status the process ends with when it finds this itself.
    pub const fn status(&self) -> i32 {
        if self.errno == -errno::ENOENT || self.errno == -errno::ENOTDIR {
            feos_abi::EXIT_NOT_FOUND
        } else {
            feos_abi::EXIT_NOT_EXECUTABLE
        }
    }
}

fn limits() -> Limits {
    Limits {
        machine: MACHINE,
        base: feos_abi::USER_BASE as u64,
        end: feos_abi::USER_IMAGE_END as u64,
        max_memory: feos_abi::USER_MEMORY_LIMIT as u64,
    }
}

/// Resolves the absolute `path` as a program for this user to run: a regular file
/// (`EACCES` for anything else), with execute permission and search on the way there
/// (`EACCES`), that's an ELF program for this machine (`ENOEXEC`). A `#!` script is
/// `ENOEXEC` until stage 1c.7 runs them.
pub fn resolve(tree: &dyn VfsTree, path: &str, uid: u32, gid: u32) -> Result<Image, Refused> {
    let found = match path::lookup(tree, path, true) {
        Ok(found) => found,
        Err(FsError::PermissionDenied) => return Err(Refused::new(errno::EACCES, "permission denied")),
        Err(FsError::NotDirectory) => return Err(Refused::new(errno::ENOTDIR, "Not a directory")),
        Err(FsError::SymlinkLoop) => return Err(Refused::new(errno::ELOOP, "too many links")),
        Err(_) => return Err(Refused::new(errno::ENOENT, "No such file or directory")),
    };
    if let Err(FsError::PermissionDenied) = tree.check_access(&found.canonical, uid, gid, ACCESS_EXECUTE) {
        return Err(Refused::new(errno::EACCES, "permission denied"));
    }
    if found.stat.kind != FileKind::File {
        return Err(Refused::new(errno::EACCES, "not a file"));
    }
    let mut head = [0u8; feos_elf::HEADER_BYTES];
    let got = tree.read_file(found.id, 0, &mut head).map_err(|_| Refused::new(errno::EIO, "can't be read"))?;
    if head[..got].starts_with(b"#!") {
        return Err(Refused::new(errno::ENOEXEC, "scripts can't be run yet"));
    }
    if let Err(error) = feos_elf::parse(&head[..got], found.stat.size, &limits()) {
        return Err(Refused { errno: -errno::ENOEXEC, message: error.text() });
    }
    let hold = pagecache::file_for(found.id, found.stat.size).ok();
    Ok(Image { canonical: found.canonical, id: found.id, size: found.stat.size, _hold: hold })
}

/// What a new program is loaded from and starts with, made before the scheduler's lock
/// is taken and given to the task in its slot (see `sched::spawn_entry`): on the heap,
/// since its paths may each be `PATH_MAX` long.
pub struct UserStart {
    /// The absolute path for the task to resolve, if `image` isn't there.
    path: String,
    /// What `SPAWN2` resolved.
    image: Option<Image>,
    /// The path as given, for `AT_EXECFN`.
    given: String,
    /// Where relative paths start, if `files` doesn't say.
    cwd: String,
    /// What it starts with: its parent program's, or [`DEFAULT_UMASK`](super::DEFAULT_UMASK).
    umask: u32,
    strings: Staged,
    /// Its descriptor table and working directory, as `SPAWN2` built them; otherwise
    /// the standard streams.
    files: Option<Files>,
}

/// A copy of `text` on the heap, fallibly.
pub(super) fn copy(text: &str) -> Option<String> {
    let mut out = String::new();
    out.try_reserve_exact(text.len()).ok()?;
    out.push_str(text);
    Some(out)
}

impl UserStart {
    /// What a kernel app starts the program at `path` with: `argv`, `env` and `cwd`. Too
    /// much of them together is `ArgsTooLong`, as a program's `SPAWN2` would be `E2BIG`.
    pub fn new(path: &str, argv: &ArgBlock, env: &[&str], cwd: &str, umask: u32) -> Result<Box<Self>, SpawnError> {
        let pointers = (argv.iter().count() + env.len()) * 8;
        let bytes: usize = argv.iter().chain(env.iter().copied()).map(|text| text.len() + 1).sum();
        if bytes + pointers > spawn::ARG_MAX {
            return Err(SpawnError::ArgsTooLong);
        }
        let mut strings = Staged::new(bytes, None).map_err(|_| SpawnError::NoMemory)?;
        for arg in argv.iter() {
            strings.push(arg.as_bytes(), false);
        }
        for entry in env {
            strings.push(entry.as_bytes(), true);
        }
        Self::make(path, None, path, cwd, umask, strings, None).ok_or(SpawnError::NoMemory)
    }

    /// What a program starts a program with: from `SPAWN2`, `image` and `files`; from v1's
    /// `SPAWN`, the path for the task to resolve.
    pub fn make(
        path: &str,
        image: Option<Image>,
        given: &str,
        cwd: &str,
        umask: u32,
        strings: Staged,
        files: Option<Files>,
    ) -> Option<Box<Self>> {
        Some(Box::new(Self { path: copy(path)?, image, given: copy(given)?, cwd: copy(cwd)?, umask, strings, files }))
    }
}

/// Loads the program `start` names and runs it. Only returns if it can't, with the
/// status the process should end with. The process is the slot's from the moment it
/// exists, so a failure or a kill anywhere in here still gets its frames back.
pub fn start(slot: usize, start: Box<UserStart>) -> i32 {
    let loaded = load(slot, start);
    match loaded {
        Ok((process, frame)) => sched::enter_user(slot, process.mm().root(), process.mm().tlb(), frame),
        Err(status) => status,
    }
}

fn load(slot: usize, start: Box<UserStart>) -> Result<(&'static Process, Frame), i32> {
    let UserStart { path, image, given, cwd, umask, strings, files } = *start;
    let Some(tree) = tree() else {
        say(format_args!("{path}: no filesystem"));
        return Err(feos_abi::EXIT_NOT_FOUND);
    };
    let (uid, gid) = sched::credentials(slot);
    let image = match image {
        Some(image) => image,
        None => resolve(tree, &path, uid, gid).map_err(|refused| {
            say(format_args!("{path}: {}", refused.message));
            refused.status()
        })?,
    };
    // Read again from the file held: `SPAWN2` checked it, but the file may have been
    // written since, and what's loaded is what's read now.
    let mut head = [0u8; feos_elf::HEADER_BYTES];
    let got = tree.read_file(image.id, 0, &mut head).map_err(|_| {
        say(format_args!("{path}: can't be read"));
        feos_abi::EXIT_NOT_EXECUTABLE
    })?;
    let program = feos_elf::parse(&head[..got], image.size, &limits()).map_err(|error| {
        say(format_args!("{path}: {}", error.text()));
        feos_abi::EXIT_NOT_EXECUTABLE
    })?;

    // The working directory is canonical: links resolved, as `GETCWD` sees it. One that
    // can't be resolved is kept as given. `SPAWN2`'s child has its own already.
    let home = match &files {
        Some(_) => None,
        None => tree.resolve_path(&cwd, true).ok().map(|found| found.canonical),
    };
    let made = process::create(slot, super::current_pid(), home.as_deref().unwrap_or(&cwd), umask, &image.canonical, files);
    let Some(process) = made else {
        say(format_args!("{path}: out of memory"));
        return Err(feos_abi::EXIT_NOT_EXECUTABLE);
    };
    load_segments(process, tree, &path, &image, &program)?;

    // The argument area at the top, and the stack directly below it: separate, so the
    // whole stack is free at `main` however long the arguments are, and a program
    // running down from its first stack pointer crosses the rest of the argument area
    // into the stack with nothing between. Both are reserved whole and charged as
    // they're touched, as `MAP_NORESERVE` memory is: promised whole, 10 MiB a program,
    // a 512 MiB machine could start about twenty, whatever they used. A page touched
    // when there's no frame to be had ends the program with `SIGBUS`, as any
    // `MAP_NORESERVE` page would.
    let user = process.mm();
    let args_low = feos_abi::USER_STACK_TOP - feos_abi::USER_ARGS_SIZE;
    let stack_low = args_low - feos_abi::USER_STACK_SIZE;
    for (low, length) in [(args_low, feos_abi::USER_ARGS_SIZE), (stack_low, feos_abi::USER_STACK_SIZE)] {
        let request = MapRequest {
            place: Place::Exact(low),
            noreserve: true,
            ..MapRequest::anonymous(length, Prot::READ_WRITE)
        };
        if user.with(|mm, ctx| mm.map(&request, ctx)).is_err() {
            say(format_args!("{path}: out of memory"));
            return Err(feos_abi::EXIT_NOT_EXECUTABLE);
        }
    }

    let Ok(place) = build_stack(slot, process, &strings, &given, &image.canonical, &program) else {
        say(format_args!("{path}: out of memory for its arguments"));
        return Err(feos_abi::EXIT_NOT_EXECUTABLE);
    };
    // The staged strings are in the program now.
    drop(strings);
    drop(image);
    Port::sync_user_tables();
    Ok((process, Frame::new_user(program.entry as usize, place.sp, place.argc, place.strings)))
}

/// Maps and loads the program's segments.
fn load_segments(process: &Process, tree: &dyn VfsTree, path: &str, image: &Image, program: &feos_elf::Program) -> Result<(), i32> {
    let user = process.mm();
    // Code and read-only data are mapped from the page cache, so every copy of a program
    // runs the same frames, and the file can't be written while it runs. That takes a
    // segment whose file offset and address agree within a page, and no zeroed tail;
    // anything else, writable data included, is copied into frames of its own.
    let cache = pagecache::file_for(image.id, image.size).ok();
    for segment in program.segments() {
        let prot = Prot {
            read: true,
            write: segment.writable,
            exec: segment.executable,
        };
        let start = segment.page_start() as usize;
        let length = (segment.page_end() - segment.page_start()) as usize;
        let page = PAGE_SIZE as u64;
        let cached = cache.as_ref().filter(|_| {
            !segment.writable && segment.offset % page == segment.address % page && segment.memory_size == segment.file_size
        });
        if let Some(file) = cached {
            let request = MapRequest {
                place: Place::Exact(start),
                // Every page in now: a program's text is read once, and its pages are
                // there even if the file is later removed.
                populate: true,
                file: Some(FileMap { file: file.clone(), first: segment.offset / page, may_write: false }),
                ..MapRequest::anonymous(length, prot)
            };
            match user.with(|mm, ctx| mm.map(&request, ctx)) {
                Ok(_) => continue,
                Err(feos_vm::MmError::TextBusy) => {
                    say(format_args!("{path}: text file busy"));
                    return Err(feos_abi::EXIT_NOT_EXECUTABLE);
                }
                Err(_) => {
                    say(format_args!("{path}: out of memory, or overlapping segments"));
                    return Err(feos_abi::EXIT_NOT_EXECUTABLE);
                }
            }
        }
        // Each page gets a frame of its own now, read-only ones too, to load into.
        let request = MapRequest {
            place: Place::Exact(start),
            image: true,
            ..MapRequest::anonymous(length, prot)
        };
        if user.with(|mm, ctx| mm.map(&request, ctx)).is_err() {
            say(format_args!("{path}: out of memory, or overlapping segments"));
            return Err(feos_abi::EXIT_NOT_EXECUTABLE);
        }
        // Loaded with the space's lock held: the filesystem comes after it, and
        // nothing else is using this space yet.
        let state = user.lock();
        let mut done = 0u64;
        while done < segment.file_size {
            let address = segment.address as usize + done as usize;
            let Ok(target) = state.mm.space().translate(address, false) else {
                return Err(feos_abi::EXIT_NOT_EXECUTABLE);
            };
            let count = ((PAGE_SIZE - address % PAGE_SIZE) as u64).min(segment.file_size - done) as usize;
            // SAFETY: `target` is a frame of this new space's own, which nothing can run
            // yet, and `count` stops at its end.
            let out = unsafe { core::slice::from_raw_parts_mut(target as *mut u8, count) };
            match tree.read_file(image.id, segment.offset + done, out) {
                Ok(got) if got > 0 => done += got as u64,
                _ => {
                    say(format_args!("{path}: truncated"));
                    return Err(feos_abi::EXIT_NOT_EXECUTABLE);
                }
            }
        }
        if segment.executable {
            let mut page = start;
            while page < start + length {
                if let Ok(frame) = state.mm.space().translate(page, false) {
                    Port::sync_user_code(frame, PAGE_SIZE);
                }
                page += PAGE_SIZE;
            }
        }
    }
    Ok(())
}

/// Where a new program's first stack is.
struct StackPlace {
    argc: usize,
    /// The `UserStr` array, for v1's convention.
    strings: usize,
    /// The first stack pointer: the `argc` word.
    sp: usize,
}

/// Words written to a program's memory in order, a page's worth at a time.
struct Words<'a> {
    user: &'a UserMm,
    at: usize,
    buffer: [u8; 1024],
    used: usize,
}

impl<'a> Words<'a> {
    fn new(user: &'a UserMm, at: usize) -> Self {
        Self { user, at, buffer: [0; 1024], used: 0 }
    }

    fn push(&mut self, word: u64) -> Result<(), isize> {
        if self.used == self.buffer.len() {
            self.flush()?;
        }
        self.buffer[self.used..self.used + 8].copy_from_slice(&word.to_ne_bytes());
        self.used += 8;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), isize> {
        self.user.copy_out(self.at, &self.buffer[..self.used])?;
        self.at += self.used;
        self.used = 0;
        Ok(())
    }
}

/// 16 bytes for `AT_RANDOM`. There's no CSPRNG before stage 1e.1, and `AT_FEOS_FLAGS`
/// says so: these are only different for every program.
fn random_bytes(pid: u32) -> [u8; 16] {
    let mut state = crate::time::monotonic_ns() ^ (u64::from(pid) << 32) ^ 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    };
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&next().to_ne_bytes());
    out[8..].copy_from_slice(&next().to_ne_bytes());
    out
}

/// Lays out the System V initial stack at the top of the argument area (section 7.1 of
/// the ABI design): from the stack pointer up, `argc`, the `argv` pointers and a zero,
/// the `envp` pointers and a zero, the aux vector, the `UserStr`s of the arguments for
/// v1's convention, the random bytes, and the strings.
fn build_stack(slot: usize, process: &Process, strings: &Staged, given: &str, exe: &str, program: &feos_elf::Program) -> Result<StackPlace, isize> {
    let user = process.mm();
    let top = feos_abi::USER_STACK_TOP;
    let low = top - feos_abi::USER_ARGS_SIZE;
    let text_length = strings.length + given.len() + 1 + exe.len() + 1 + PLATFORM.len() + 1;
    let text = top.checked_sub(text_length).ok_or(-errno::E2BIG)? & !15;
    let random = text - 16;
    let (argc, envc) = (strings.argc, strings.envc);
    let array = (random - argc * core::mem::size_of::<UserStr>()) & !15;

    let execfn = text + strings.length;
    let exe_at = execfn + given.len() + 1;
    let platform = exe_at + exe.len() + 1;
    let (uid, gid) = sched::credentials(slot);
    let (hwcap, hwcap2) = Port::user_hwcaps();
    let mut vector = [(aux::NULL, 0u64); AUX_ENTRIES];
    let entries = [
        (aux::PHDR, program.headers),
        (aux::PHENT, 56),
        (aux::PHNUM, u64::from(program.header_count)),
        (aux::PAGESZ, PAGE_SIZE as u64),
        (aux::FLAGS, 0),
        (aux::ENTRY, program.entry),
        (aux::UID, u64::from(uid)),
        (aux::EUID, u64::from(uid)),
        (aux::GID, u64::from(gid)),
        (aux::EGID, u64::from(gid)),
        (aux::PLATFORM, platform as u64),
        (aux::HWCAP, hwcap),
        (aux::HWCAP2, hwcap2),
        (aux::CLKTCK, 100),
        (aux::SECURE, 0),
        (aux::RANDOM, random as u64),
        (aux::EXECFN, execfn as u64),
        (aux::MINSIGSTKSZ, SIGNAL_FRAME_MAX),
        (aux::FEOS_ABI, u64::from(feos_abi::ABI_VERSION)),
        (aux::FEOS_FLAGS, 0),
        (aux::FEOS_FEATURES, feos_abi::FEATURES),
        (aux::FEOS_EXE, exe_at as u64),
    ];
    vector[..entries.len()].copy_from_slice(&entries);
    let words = 1 + argc + 1 + envc + 1 + 2 * (entries.len() + 1);
    let sp = array.checked_sub(words * 8).ok_or(-errno::E2BIG)? & !15;
    if sp < low {
        return Err(-errno::E2BIG);
    }

    // The strings, the program's two paths and `AT_PLATFORM`, each with its NUL.
    strings.copy_out(user, text)?;
    for (at, string) in [(execfn, given), (exe_at, exe), (platform, PLATFORM)] {
        user.copy_out(at, string.as_bytes())?;
        user.copy_out(at + string.len(), &[0])?;
    }
    user.copy_out(random, &random_bytes(process.pid()))?;

    let mut out = Words::new(user, sp);
    out.push(argc as u64)?;
    let mut index = 0;
    strings.each_string(|offset, _| {
        if index == argc {
            out.push(0)?;
        }
        index += 1;
        out.push((text + offset) as u64)
    })?;
    if index == argc {
        out.push(0)?;
    }
    out.push(0)?;
    for &(kind, value) in &vector[..=entries.len()] {
        out.push(kind)?;
        out.push(value)?;
    }
    out.flush()?;

    let mut out = Words::new(user, array);
    let mut index = 0;
    strings.each_string(|offset, length| {
        if index < argc {
            out.push((text + offset) as u64)?;
            out.push(length as u64)?;
        }
        index += 1;
        Ok(())
    })?;
    out.flush()?;
    Ok(StackPlace { argc, strings: array, sp })
}
