//! Tries everything a user can and can't do to a filesystem with owners, from user mode.
//!     permtest <directory on a native filesystem>
//!     permtest --permissionless <directory on anything else>
//! It starts as root, makes what it needs, then becomes user 1000. Exits zero when every
//! answer was what it should be, naming any that wasn't.

#![no_std]
#![no_main]

use feos_user::abi::{access, at, errno, open as flags, Timespec, KEEP};
use feos_user::{
    chmod, chown, close, entry, eprintln, error_name, faccessat, fchmodat, fchownat, getgid, getuid, link, linkat, mkdir,
    open, println, read, readlinkat, renameat, setuid, spawn, stat, symlinkat, unlink, utimensat, wait, write, Args,
};

const CWD: i32 = at::FDCWD;

struct Paths {
    buffer: [u8; 2048],
    used: usize,
}

impl Paths {
    /// `directory/name`, for the life of the program.
    fn join(&mut self, directory: &str, name: &str) -> &'static str {
        let start = self.used;
        for byte in directory.bytes().chain(core::iter::once(b'/')).chain(name.bytes()) {
            self.buffer[self.used] = byte;
            self.used += 1;
        }
        // SAFETY: the only `Paths` is a static that lives as long as the program, written
        // only past what's been handed out, and these are valid UTF-8 pieces of UTF-8.
        unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(self.buffer.as_ptr().add(start), self.used - start))
        }
    }
}

static mut PATHS: Paths = Paths { buffer: [0; 2048], used: 0 };

fn path(directory: &str, name: &str) -> &'static str {
    // SAFETY: one thread, and no reference is kept to the struct itself.
    unsafe { (*core::ptr::addr_of_mut!(PATHS)).join(directory, name) }
}

struct Score {
    checks: u32,
    failures: u32,
}

impl Score {
    fn expect(&mut self, what: &str, got: isize, wanted: isize) {
        self.checks += 1;
        // Anything at or above zero is a success, as a descriptor is.
        let matches = if wanted >= 0 { got >= 0 } else { got == wanted };
        if !matches {
            self.failures += 1;
            eprintln!("permtest: {what}: got {} ({got}), wanted {wanted}", error_name(got));
        }
    }
}

fn opened(path: &str, how: usize) -> isize {
    let fd = open(path, how);
    if fd >= 0 {
        close(fd as usize);
    }
    fd
}

fn put(path: &str, text: &[u8]) -> isize {
    let fd = open(path, flags::WRITE | flags::CREATE | flags::TRUNCATE);
    if fd < 0 {
        return fd;
    }
    let wrote = write(fd as usize, text);
    close(fd as usize);
    wrote
}

fn permissionless(directory: &str) -> i32 {
    let mut score = Score { checks: 0, failures: 0 };
    let file = path(directory, "anyone");
    score.expect("a file as root", put(file, b"x"), 0);
    let before = stat(file);
    score.expect("stat", before.map_or_else(|error| error, |_| 0), 0);
    if let Ok(info) = before {
        score.checks += 1;
        // Modes are made up, and say they are.
        if info.posix != 0 || info.uid != 0 || info.mode & 0o777 == 0 {
            score.failures += 1;
            eprintln!("permtest: mode {:o} uid {} posix {}", info.mode, info.uid, info.posix);
        }
    }
    score.expect("becoming 1000", setuid(1000, 1000), 0);
    // Nobody's stopped here, as nothing here knows who anyone is.
    score.expect("writing a root file", put(file, b"y"), 0);
    score.expect("chmod", chmod(file, 0o600), -errno::ENOTSUP);
    score.expect("removing", unlink(file), 0);
    println!("permtest: permissionless: {} checks, {} failed", score.checks, score.failures);
    i32::from(score.failures != 0)
}

fn native(directory: &str) -> i32 {
    let mut score = Score { checks: 0, failures: 0 };
    score.expect("starts as root", getuid(), 0);

    let root_dir = path(directory, "rootdir");
    let shared = path(directory, "shared");
    let secret = path(directory, "secret");
    let public = path(directory, "public");
    let script = path(directory, "notrunnable");
    score.expect("mkdir", mkdir(root_dir), 0);
    score.expect("mkdir", mkdir(shared), 0);
    score.expect("chmod shared", chmod(shared, 0o777), 0);
    score.expect("secret", put(secret, b"top secret"), 0);
    score.expect("chmod secret", chmod(secret, 0o600), 0);
    score.expect("public", put(public, b"for anyone"), 0);
    score.expect("chmod public", chmod(public, 0o644), 0);
    score.expect("not runnable", put(script, b"\x7fELF"), 0);
    score.expect("chmod not runnable", chmod(script, 0o644), 0);
    score.expect("a file for 1000 to own", put(path(directory, "theirs"), b"theirs"), 0);
    score.expect("giving it away", chown(path(directory, "theirs"), 1000, KEEP), 0);
    match stat(secret) {
        Ok(info) => {
            score.checks += 1;
            if info.posix != 1 || info.uid != 0 || info.mode & 0o7777 != 0o600 || info.mode & 0o170000 != 0o100000 {
                score.failures += 1;
                eprintln!("permtest: secret is mode {:o} uid {} posix {}", info.mode, info.uid, info.posix);
            }
        }
        Err(error) => score.expect("stat secret", error, 0),
    }

    // Root isn't stopped by a mode, except that it can't run what nobody may.
    score.expect("root reads what's 0600 and not its own", opened(secret, flags::READ), 0);
    let pid = spawn(script, &[]);
    if pid >= 0 {
        score.expect("root can't run a 0644 file", wait(pid as usize), 126);
    }

    // Now somebody else.
    score.expect("becoming 1000", setuid(1000, 100), 0);
    score.expect("uid", getuid(), 1000);
    score.expect("gid", getgid(), 100);
    score.expect("reading 0600 of root's", opened(secret, flags::READ), -errno::EACCES);
    score.expect("writing 0600 of root's", opened(secret, flags::WRITE), -errno::EACCES);
    score.expect("reading 0644 of root's", opened(public, flags::READ), 0);
    score.expect("writing 0644 of root's", opened(public, flags::WRITE), -errno::EACCES);
    score.expect("truncating 0644 of root's", opened(public, flags::WRITE | flags::TRUNCATE), -errno::EACCES);
    let mut byte = [0u8; 1];
    let fd = open(public, flags::READ);
    if fd >= 0 {
        score.expect("what it could read", read(fd as usize, &mut byte), 1);
        close(fd as usize);
    }
    score.expect("making a file in root's 0755", opened(path(root_dir, "no"), flags::WRITE | flags::CREATE), -errno::EACCES);
    score.expect("making a directory in root's 0755", mkdir(path(root_dir, "no")), -errno::EACCES);
    score.expect("removing from root's 0755", unlink(public), -errno::EACCES);
    score.expect("a link in root's 0755", link(public, path(root_dir, "no")), -errno::EACCES);
    score.expect("chmod of root's", chmod(public, 0o666), -errno::EPERM);
    score.expect("chown, not being root", chown(public, 1000, KEEP), -errno::EPERM);
    // The `*at` calls, as v1's: a mode, an owner, times and names of root's are root's.
    score.expect("fchmodat of root's", fchmodat(CWD, public, 0o666, 0), -errno::EPERM);
    score.expect("fchownat, not being root", fchownat(CWD, public, 1000, KEEP, 0), -errno::EPERM);
    score.expect("fchownat leaving both", fchownat(CWD, public, KEEP, u32::MAX as usize, 0), 0);
    let old = [Timespec { sec: 1_600_000_000, nsec: 0 }; 2];
    score.expect("utimensat of root's to a time", utimensat(CWD, public, Some(&old), 0), -errno::EPERM);
    score.expect("utimensat of root's 0644 to now", utimensat(CWD, public, None, 0), -errno::EACCES);
    score.expect("faccessat reading 0600 of root's", faccessat(CWD, secret, access::R_OK, 0), -errno::EACCES);
    score.expect("faccessat reading 0644 of root's", faccessat(CWD, public, access::R_OK, 0), 0);
    score.expect("faccessat writing 0644 of root's", faccessat(CWD, public, access::W_OK, 0), -errno::EACCES);
    score.expect("faccessat running 0644 of root's", faccessat(CWD, script, access::X_OK, 0), -errno::EACCES);
    score.expect("renameat out of root's 0755", renameat(CWD, public, CWD, path(shared, "taken"), 0), -errno::EACCES);
    score.expect("renameat into root's 0755", renameat(CWD, path(directory, "theirs"), CWD, path(root_dir, "no"), 0), -errno::EACCES);
    score.expect("symlinkat in root's 0755", symlinkat("anywhere", CWD, path(root_dir, "no")), -errno::EACCES);
    score.expect("linkat into root's 0755", linkat(CWD, public, CWD, path(root_dir, "no"), 0), -errno::EACCES);
    score.expect("going back to root", setuid(0, 0), -errno::EPERM);
    let pid = spawn(script, &[]);
    if pid >= 0 {
        score.expect("running a 0644 file", wait(pid as usize), 126);
    }

    // What's its own, it can do what it likes with.
    let mine = path(shared, "mine");
    score.expect("making a file in 0777", put(mine, b"mine"), 0);
    match stat(mine) {
        Ok(info) => {
            score.checks += 1;
            if info.uid != 1000 || info.gid != 100 {
                score.failures += 1;
                eprintln!("permtest: a new file belongs to {}:{}", info.uid, info.gid);
            }
        }
        Err(error) => score.expect("stat of its own", error, 0),
    }
    score.expect("chmod of its own", chmod(mine, 0o400), 0);
    score.expect("its own 0400 for writing", opened(mine, flags::WRITE), -errno::EACCES);
    score.expect("its own 0400 for reading", opened(mine, flags::READ), 0);
    score.expect("a hard link in 0777", link(mine, path(shared, "again")), 0);
    match stat(path(shared, "again")) {
        Ok(info) => score.expect("two links", info.nlink as isize, 2),
        Err(error) => score.expect("stat of the link", error, 0),
    }
    score.expect("removing one", unlink(path(shared, "again")), 0);
    score.expect("linkat in 0777", linkat(CWD, mine, CWD, path(shared, "linked"), 0), 0);
    score.expect("renameat its own over its own", renameat(CWD, path(shared, "linked"), CWD, path(shared, "moved"), 0), 0);
    score.expect("symlinkat in 0777", symlinkat("mine", CWD, path(shared, "sym")), 0);
    let mut target = [0u8; 16];
    score.expect("readlinkat of it", readlinkat(CWD, path(shared, "sym"), &mut target), 4);
    match feos_user::fstatat(CWD, path(shared, "sym"), at::SYMLINK_NOFOLLOW) {
        Ok(info) => score.expect("the link is its own", info.uid as isize, 1000),
        Err(error) => score.expect("lstat of the link", error, 0),
    }
    score.expect("fchmodat of its own", fchmodat(CWD, mine, 0o600, 0), 0);
    score.expect("utimensat of its own", utimensat(CWD, mine, Some(&old), 0), 0);
    score.expect("faccessat of its own 0600", faccessat(CWD, mine, access::R_OK | access::W_OK, 0), 0);
    score.expect("a directory of its own", mkdir(path(shared, "mydir")), 0);
    score.expect("the file given to it", put(path(directory, "theirs"), b"changed"), 0);

    // Getting there counts as well: a directory nobody may go through, closed by its owner.
    score.expect("closing its own directory", chmod(path(shared, "mydir"), 0o000), 0);
    score.expect("through a closed directory", opened(path(path(shared, "mydir"), "x"), flags::WRITE | flags::CREATE), -errno::EACCES);
    score.expect("stat through a closed directory", stat(path(path(shared, "mydir"), "x")).map_or_else(|error| error, |_| 0), -errno::EACCES);

    println!("permtest: native: {} checks, {} failed", score.checks, score.failures);
    i32::from(score.failures != 0)
}

fn main(args: Args) -> i32 {
    match (args.get(1), args.get(2)) {
        (Some("--permissionless"), Some(directory)) => permissionless(directory),
        (Some(directory), None) => native(directory),
        _ => {
            eprintln!("usage: permtest [--permissionless] DIRECTORY");
            2
        }
    }
}

entry!(main);
