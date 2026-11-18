//! Runs through descriptors and transfers (stage 1c.1): open file descriptions shared by
//! `DUP3` and `F_DUPFD`, close-on-exec, `F_GETFL` and `F_SETFL`, `PIPE2`'s flags, a
//! pipe's end of file and broken pipe, writes bigger than a pipe or an `IO_CHUNK` done in
//! one call, gaps that read as zeroes, `CLOSE_RANGE`, a thousand descriptors and a path
//! near `PATH_MAX`. Then opening, looking and directories (stage 1c.2): `OPENAT` relative
//! to a `dirfd`, its flags and POSIX's trailing slashes, physical `..`, `O_PATH`,
//! `FSTATAT`, `GETDENTS` and its cookies, the umask, `UNLINKAT` and files that keep
//! working once unlinked, `FTRUNCATE`, `PREAD`/`PWRITE`, `FSYNC` of a mapping, the
//! working directory, `FD_PATH` and `CURRENT_EXE`. Then `/dev`, `/proc/self` and the
//! image's layout (stage 1c.3): the device nodes, `/dev/zero` mapped, the links of
//! `/dev/fd` and `/dev/stdout`, `/dev/shm`, `/proc/self/exe` and `/proc/self/fd`, the
//! sticky `/tmp` and `/etc`'s files. Then changing the tree, and times (stage 1c.4):
//! `RENAMEAT` over what's there, in use or running, `RENAME_NOREPLACE`, `EXDEV` and a
//! directory under itself; `LINKAT`, `SYMLINKAT`, `READLINKAT`; modes, owners and times
//! through paths and descriptors, to the nanosecond where the filesystem keeps them, and
//! the rules for when each changes; `FACCESSAT`, `FSTATVFS` and `SYNC`. Then locks (stage
//! 1c.5): `FLOCK` between descriptions and processes, waiting and at exit, and `fcntl`'s
//! record locks between processes, `F_GETLK`, closes, `struct flock`'s padding and the
//! byte locks SQLite takes. Reports each check, and exits zero if all passed.
//!
//!     ftest [--dir DIR]           files go in DIR (default /tmp), so that a boot test can
//!                                 run it on each kind of filesystem. Symbolic links
//!                                 (there's no call to make one until stage 1c.4) are the
//!                                 boot test's: DIR/ftest-hop to DIR/ftest-real/sub, and
//!                                 DIR/ftest-flink to DIR/ftest-real/file
//!     ftest --dev-fd              writes through /dev/fd/1 and /dev/stdout, for
//!                                 `dev-fd-dups`
//!     ftest --console-user        as uid 1000, reaches a console descriptor through
//!                                 its links, for `dev-console-as-user`
//!     ftest --sticky PATH         as uid 1000, tries to remove root's PATH in /tmp, for
//!                                 `tmp-sticky`
//!     ftest --chown-user PATH     as uid 1000, tries to give PATH away, for
//!                                 `fchownat-root-only`
//!     ftest --persist-write DIR   renames a file over another and sets its times to the
//!     ftest --persist-check DIR   nanosecond; after a reboot, checks they're there
//!     --coarse, --hard-links      what DIR's filesystem keeps: FAT's two-second times
//!                                 and no owners or modes; the native format's hard links
//!     ftest --setlk PATH KIND START LENGTH, --getlk PATH START LENGTH KIND PID,
//!           --flock-wait PATH, --flock-exit PATH, --sqlite-shared PATH, --sqlite-hold PATH
//!                                 the other processes of the lock checks
//!     ftest --drain BYTES         reads standard input a little at a time, checking the
//!                                 pattern `pipe-large-write-completes` writes: exits 0
//!                                 if exactly BYTES came, intact

#![no_std]
#![no_main]

use core::fmt::Write as _;

use feos_user::abi::{self, at, close_range as range, errno, f, lock, map, o, open as flags, prot, rename, s_if, seek as whence, Dirent, PAGE_SIZE};
use feos_user::{
    close, close_range, current_exe, dup, dup3, entry, faccessat, fchdir, fchmodat, fchownat, fcntl, fcntl_lock, fd_path, flock, fstat, fstatat,
    fstatvfs, fsync, ftruncate, getcwd, getdents, linkat, mkdirat, mmap2, mmap2_file, munmap, open, openat, parse_number,
    pipe, pipe2, pread, println, pwrite, read, readlinkat, record, renameat, seek, sleep_ms, spawn, spawn_io, stat, symlinkat, umask,
    unlink, unlinkat, utimensat, wait, write, Args,
};

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, good: bool, detail: core::fmt::Arguments<'_>) {
        if good {
            println!("ftest {name}: ok ({detail})");
        } else {
            println!("ftest {name}: FAILED ({detail})");
            self.failed += 1;
        }
    }
}

/// The byte at `index` of what the big transfers write: never the same for long, so a
/// piece out of place shows.
fn pattern(index: usize) -> u8 {
    (index.wrapping_mul(31) ^ (index >> 11)) as u8
}

/// A path in the test directory, in `out`.
fn path_in<'a>(out: &'a mut [u8; 256], dir: &str, name: &str) -> &'a str {
    let mut length = 0;
    for part in [dir.as_bytes(), b"/", name.as_bytes()] {
        out[length..length + part.len()].copy_from_slice(part);
        length += part.len();
    }
    core::str::from_utf8(&out[..length]).unwrap_or("/")
}

/// Anonymous memory for a big buffer, or nothing.
fn buffer(length: usize) -> Option<&'static mut [u8]> {
    let address = mmap2(0, length, prot::READ | prot::WRITE, map::PRIVATE);
    if address < 0 {
        return None;
    }
    // SAFETY: just mapped, readable and writable, and only this slice refers to it.
    Some(unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) })
}

fn give_back(memory: &mut [u8]) {
    munmap(memory.as_mut_ptr() as usize, memory.len());
}

fn test_dup(report: &mut Report, dir: &str) {
    let mut name = [0u8; 256];
    let path = path_in(&mut name, dir, "ftest-dup");
    let fd = open(path, flags::READ | flags::WRITE | flags::CREATE | flags::TRUNCATE);
    if fd < 0 {
        report.check("dup3-shares-offset", false, format_args!("open: {fd}"));
        return;
    }
    let fd = fd as usize;
    write(fd, b"abcdefghij");
    seek(fd, 0, whence::SET);
    // One description, two descriptors: a read through either moves both.
    let copy = dup3(fd, 40, 0);
    let mut out = [0u8; 4];
    let first = read(fd, &mut out[..3]);
    let second = read(40, &mut out[..3]);
    let at = seek(fd, 0, whence::CURRENT);
    report.check(
        "dup3-shares-offset",
        copy == 40 && first == 3 && second == 3 && &out[..3] == b"def" && at == 6,
        format_args!("dup3={copy} reads={first},{second} then at {at}"),
    );

    // Its own close-on-exec flag, the original's untouched; and onto an open descriptor,
    // which closes first.
    let marked = dup3(fd, 41, o::CLOEXEC);
    let flags_new = fcntl(41, f::GETFD, 0);
    let flags_old = fcntl(fd, f::GETFD, 0);
    let over = dup3(41, 40, 0);
    let over_flag = fcntl(40, f::GETFD, 0);
    let cleared = fcntl(41, f::SETFD, 0) == 0 && fcntl(41, f::GETFD, 0) == 0;
    let set = fcntl(fd, f::SETFD, f::FD_CLOEXEC) == 0 && fcntl(fd, f::GETFD, 0) == f::FD_CLOEXEC as isize;
    fcntl(fd, f::SETFD, 0);
    report.check(
        "dup3-cloexec",
        marked == 41 && flags_new == f::FD_CLOEXEC as isize && flags_old == 0 && over == 40 && over_flag == 0 && cleared && set,
        format_args!("new={flags_new} old={flags_old} over={over}:{over_flag} cleared={cleared} set={set}"),
    );
    close(40);
    close(41);

    // `F_DUPFD`: the lowest free at or above the floor, and `dup` the lowest of all.
    let a = fcntl(fd, f::DUPFD, 20);
    let b = fcntl(fd, f::DUPFD, 20);
    let c = fcntl(fd, f::DUPFD_CLOEXEC, 20);
    close(21);
    let again = fcntl(fd, f::DUPFD, 20);
    let lowest = dup(fd);
    let c_flag = fcntl(22, f::GETFD, 0);
    report.check(
        "fcntl-dupfd-lowest",
        a == 20 && b == 21 && c == 22 && again == 21 && c_flag == f::FD_CLOEXEC as isize && (3..20).contains(&lowest),
        format_args!("{a} {b} {c}, then {again}; dup={lowest}"),
    );
    for extra in [20, 21, 22, lowest as usize] {
        close(extra);
    }

    // `F_SETFL(F_GETFL | O_NONBLOCK)` works, and changes nothing else; `O_APPEND` sends
    // writes to the end.
    let before = fcntl(fd, f::GETFL, 0);
    let set = fcntl(fd, f::SETFL, before as usize | o::NONBLOCK);
    let after = fcntl(fd, f::GETFL, 0);
    let append = fcntl(fd, f::SETFL, o::APPEND | o::RDONLY | o::CREAT);
    seek(fd, 0, whence::SET);
    write(fd, b"Z");
    let end = seek(fd, 0, whence::CURRENT);
    let appended = fcntl(fd, f::GETFL, 0);
    let read_only = open(path, flags::READ);
    let read_only_flags = if read_only >= 0 { fcntl(read_only as usize, f::GETFL, 0) } else { read_only };
    report.check(
        "fcntl-getfl-setfl",
        before == o::RDWR as isize
            && set == 0
            && after == (o::RDWR | o::NONBLOCK) as isize
            && append == 0
            && end == 11
            && appended == (o::RDWR | o::APPEND) as isize
            && read_only_flags == o::RDONLY as isize,
        format_args!("{before:#o} -> {after:#o}; append={append} end={end} {appended:#o}; read-only {read_only_flags:#o}"),
    );
    if read_only >= 0 {
        close(read_only as usize);
    }
    close(fd);
    unlink(path);
}

fn test_pipes(report: &mut Report, program: &str) {
    // Non-blocking both ways: `EAGAIN` empty, `EAGAIN` full, and at least 16 KiB held.
    match pipe2(o::NONBLOCK) {
        Ok((reader, writer)) => {
            let mut out = [0u8; 8];
            let empty = read(reader, &mut out);
            // Pieces of at most `PIPE_BUF` go in whole or not at all, so these fill it exactly.
            let block = [7u8; 1024];
            let mut held = 0usize;
            let full = loop {
                let wrote = write(writer, &block);
                if wrote <= 0 {
                    break wrote;
                }
                held += wrote as usize;
            };
            let read_flags = fcntl(reader, f::GETFL, 0);
            report.check(
                "pipe2-nonblock-eagain",
                empty == -errno::EAGAIN && full == -errno::EAGAIN && held >= 16 * 1024 && read_flags == (o::RDONLY | o::NONBLOCK) as isize,
                format_args!("empty={empty} full={full} after {held} bytes, flags {read_flags:#o}"),
            );
            close(reader);
            close(writer);
        }
        Err(error) => report.check("pipe2-nonblock-eagain", false, format_args!("pipe2: {error}")),
    }

    match pipe2(o::CLOEXEC) {
        Ok((reader, writer)) => {
            let (r, w) = (fcntl(reader, f::GETFD, 0), fcntl(writer, f::GETFD, 0));
            let plain = pipe().map(|(a, b)| {
                let flags = (fcntl(a, f::GETFD, 0), fcntl(b, f::GETFD, 0));
                close(a);
                close(b);
                flags
            });
            report.check(
                "pipe2-cloexec",
                r == f::FD_CLOEXEC as isize && w == f::FD_CLOEXEC as isize && plain == Ok((0, 0)),
                format_args!("ends {r},{w}; plain pipe {plain:?}"),
            );
            close(reader);
            close(writer);
        }
        Err(error) => report.check("pipe2-cloexec", false, format_args!("pipe2: {error}")),
    }

    // Two descriptors for the write end: the reader sees end of file only once both close.
    match pipe2(o::NONBLOCK) {
        Ok((reader, writer)) => {
            let second = dup(writer);
            write(writer, b"xy");
            close(writer);
            let mut out = [0u8; 8];
            let data = read(reader, &mut out);
            let open_still = read(reader, &mut out);
            close(second as usize);
            let eof = read(reader, &mut out);
            report.check(
                "pipe-eof-after-last-writer",
                second > 0 && data == 2 && open_still == -errno::EAGAIN && eof == 0,
                format_args!("data={data} with a writer left={open_still} after the last={eof}"),
            );
            close(reader);
        }
        Err(error) => report.check("pipe-eof-after-last-writer", false, format_args!("pipe2: {error}")),
    }

    match pipe() {
        Ok((reader, writer)) => {
            close(reader);
            let small = write(writer, b"lost");
            let large = [0u8; 8192];
            let big = write(writer, &large);
            report.check("pipe-epipe", small == -errno::EPIPE && big == -errno::EPIPE, format_args!("{small} {big}"));
            close(writer);
        }
        Err(error) => report.check("pipe-epipe", false, format_args!("pipe: {error}")),
    }

    // A megabyte in one write, to a child reading a hundred bytes at a time.
    const TOTAL: usize = 1024 * 1024;
    let Some(data) = buffer(TOTAL) else {
        report.check("pipe-large-write-completes", false, format_args!("no memory"));
        return;
    };
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = pattern(index);
    }
    match pipe() {
        Ok((reader, writer)) => {
            let child = spawn_io(program, &[program, "--drain", "1048576"], reader, abi::INHERIT);
            close(reader);
            let wrote = if child > 0 { write(writer, data) } else { child };
            close(writer);
            let status = if child > 0 { wait(child as usize) } else { child };
            report.check(
                "pipe-large-write-completes",
                wrote == TOTAL as isize && status == 0,
                format_args!("wrote {wrote} in one call, the reader said {status}"),
            );
        }
        Err(error) => report.check("pipe-large-write-completes", false, format_args!("pipe: {error}")),
    }
    give_back(data);
}

/// `--drain`: reads standard input in small pieces until end of file.
fn drain(expected: usize) -> i32 {
    let mut out = [0u8; 100];
    let mut total = 0usize;
    loop {
        let got = read(0, &mut out);
        if got < 0 {
            return 2;
        }
        if got == 0 {
            break;
        }
        for &byte in &out[..got as usize] {
            if byte != pattern(total) {
                return 3;
            }
            total += 1;
        }
    }
    if total == expected {
        0
    } else {
        1
    }
}

fn test_big_files(report: &mut Report, dir: &str) {
    let mut name = [0u8; 256];
    let path = path_in(&mut name, dir, "ftest-big");
    const TOTAL: usize = 4 * 1024 * 1024;
    let (Some(data), Some(back)) = (buffer(TOTAL), buffer(TOTAL)) else {
        report.check("file-write-4mib-one-call", false, format_args!("no memory"));
        return;
    };
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = pattern(index);
    }
    let fd = open(path, flags::READ | flags::WRITE | flags::CREATE | flags::TRUNCATE);
    let wrote = if fd >= 0 { write(fd as usize, data) } else { fd };
    let rewound = if fd >= 0 { seek(fd as usize, 0, whence::SET) } else { fd };
    let got = if fd >= 0 { read(fd as usize, back) } else { fd };
    let same = got == TOTAL as isize && back == &data[..];
    let size = stat(path).map(|stat| stat.size);
    report.check(
        "file-write-4mib-one-call",
        wrote == TOTAL as isize && rewound == 0 && same && size == Ok(TOTAL as u64),
        format_args!("wrote {wrote}, read back {got} in one call, same={same}, size {size:?}"),
    );
    if fd >= 0 {
        close(fd as usize);
    }
    unlink(path);
    give_back(data);
    give_back(back);

    // A write past the end: the gap reads as zeroes.
    let path = path_in(&mut name, dir, "ftest-gap");
    let fd = open(path, flags::READ | flags::WRITE | flags::CREATE | flags::TRUNCATE);
    if fd < 0 {
        report.check("write-past-eof-zero-fills", false, format_args!("open: {fd}"));
        return;
    }
    let fd = fd as usize;
    const GAP_END: usize = 3 * PAGE_SIZE + 123;
    write(fd, b"abc");
    let moved = seek(fd, GAP_END as isize, whence::SET);
    let wrote = write(fd, b"z");
    let size = stat(path).map(|stat| stat.size);
    seek(fd, 0, whence::SET);
    let mut whole = [0xffu8; GAP_END + 8];
    let got = read(fd, &mut whole);
    let zeroes = whole[3..GAP_END].iter().all(|&byte| byte == 0);
    report.check(
        "write-past-eof-zero-fills",
        moved == GAP_END as isize && wrote == 1 && size == Ok(GAP_END as u64 + 1) && got == GAP_END as isize + 1 && zeroes
            && &whole[..3] == b"abc" && whole[GAP_END] == b'z',
        format_args!("size {size:?}, read {got}, gap all zero={zeroes}"),
    );
    close(fd);
    unlink(path);
}

fn test_directory(report: &mut Report, dir: &str) {
    let fd = open(dir, flags::READ);
    let mut out = [0u8; 16];
    let got = if fd >= 0 { read(fd as usize, &mut out) } else { fd };
    let flags_of = if fd >= 0 { fcntl(fd as usize, f::GETFL, 0) } else { fd };
    let written = open(dir, flags::READ | flags::WRITE);
    let seek_pipe = pipe().map(|(reader, writer)| {
        let result = seek(reader, 0, whence::SET);
        close(reader);
        close(writer);
        result
    });
    report.check(
        "read-dir-eisdir",
        fd >= 0 && got == -errno::EISDIR && flags_of == (o::RDONLY | o::DIRECTORY) as isize && written == -errno::EISDIR
            && seek_pipe == Ok(-errno::ESPIPE),
        format_args!("read={got} flags={flags_of:#o} for-writing={written} seek-a-pipe={seek_pipe:?}"),
    );
    if fd >= 0 {
        close(fd as usize);
    }
}

fn test_close_range(report: &mut Report) {
    for fd in 100..110 {
        dup3(1, fd, 0);
    }
    let closed = close_range(102, 105, 0);
    let states: [bool; 10] = core::array::from_fn(|index| fcntl(100 + index, f::GETFD, 0) >= 0);
    let to_the_top = close_range(107, u32::MAX as usize, 0);
    let top_gone = (107..110).all(|fd| fcntl(fd, f::GETFD, 0) == -errno::EBADF);
    let untouched = fcntl(106, f::GETFD, 0) == 0 && fcntl(1, f::GETFD, 0) == 0;
    report.check(
        "close-range",
        closed == 0 && states == [true, true, false, false, false, false, true, true, true, true] && to_the_top == 0 && top_gone && untouched,
        format_args!("open after: {states:?}, to the top={to_the_top} gone={top_gone}"),
    );
    close_range(100, 106, 0);

    for fd in 200..205 {
        dup3(1, fd, 0);
    }
    let marked = close_range(201, 203, range::CLOEXEC);
    let flags: [isize; 5] = core::array::from_fn(|index| fcntl(200 + index, f::GETFD, 0));
    report.check(
        "close-range-cloexec",
        marked == 0 && flags == [0, 1, 1, 1, 0],
        format_args!("close-on-exec after: {flags:?}"),
    );
    close_range(200, 204, 0);
}

fn test_many(report: &mut Report, dir: &str) {
    let mut name = [0u8; 256];
    let path = path_in(&mut name, dir, "ftest-many");
    let fd = open(path, flags::WRITE | flags::CREATE | flags::TRUNCATE);
    if fd >= 0 {
        write(fd as usize, b"many");
        close(fd as usize);
    }
    // A thousand opens of it, each its own description, all at once.
    let mut opened = 0;
    let mut highest = 0;
    let mut error = 0;
    for _ in 0..1000 {
        let fd = open(path, flags::READ);
        if fd < 0 {
            error = fd;
            break;
        }
        opened += 1;
        highest = highest.max(fd);
    }
    // Each with an offset of its own: the last reads from the start.
    let mut out = [0u8; 4];
    let last = if highest > 0 { read(highest as usize, &mut out) } else { -1 };
    let first = read(3, &mut out[..2]);
    let closed = close_range(3, u32::MAX as usize, 0);
    let all_gone = fcntl(highest as usize, f::GETFD, 0) == -errno::EBADF && fcntl(3, f::GETFD, 0) == -errno::EBADF;
    report.check(
        "fd-1000",
        opened == 1000 && highest == 1002 && last == 4 && first == 2 && closed == 0 && all_gone,
        format_args!("opened {opened} up to fd {highest} (then {error}), reads {last} and {first}"),
    );
    unlink(path);

    // A 4000-byte path: the directory, then "./" over and over, then a name.
    let mut long = [0u8; 4000];
    let mut length = 0;
    for &byte in dir.as_bytes().iter().chain(b"/") {
        long[length] = byte;
        length += 1;
    }
    let tail = b"ftest-long";
    while length + 2 + tail.len() <= long.len() {
        long[length..length + 2].copy_from_slice(b"./");
        length += 2;
    }
    while length + tail.len() < long.len() {
        long[length] = b'/';
        length += 1;
    }
    long[length..].copy_from_slice(tail);
    let long = core::str::from_utf8(&long).unwrap_or("/");
    let fd = open(long, flags::READ | flags::WRITE | flags::CREATE | flags::TRUNCATE);
    let wrote = if fd >= 0 { write(fd as usize, b"long way") } else { fd };
    if fd >= 0 {
        close(fd as usize);
    }
    let mut short = [0u8; 256];
    let short = path_in(&mut short, dir, "ftest-long");
    let size = stat(short).map(|stat| stat.size);
    let by_long = stat(long).map(|stat| stat.size);
    report.check(
        "path-4000",
        long.len() == 4000 && fd >= 0 && wrote == 8 && size == Ok(8) && by_long == Ok(8),
        format_args!("{} bytes: open={fd} wrote={wrote} size {size:?} by the long name {by_long:?}", long.len()),
    );
    unlink(short);

    // A relative path of exactly `PATH_MAX` bytes: the limit is on what's given, not on
    // it joined to the working directory. Up to the root first, from wherever this runs.
    let mut relative = [0u8; abi::PATH_MAX];
    let mut tail = [0u8; 256];
    let tail = path_in(&mut tail, dir.trim_start_matches('/'), "ftest-rel");
    let mut length = 0;
    for _ in 0..64 {
        relative[length..length + 3].copy_from_slice(b"../");
        length += 3;
    }
    while length + 2 + tail.len() <= relative.len() {
        relative[length..length + 2].copy_from_slice(b"./");
        length += 2;
    }
    while length + tail.len() < relative.len() {
        relative[length] = b'/';
        length += 1;
    }
    relative[length..].copy_from_slice(tail.as_bytes());
    let relative = core::str::from_utf8(&relative).unwrap_or(".");
    let fd = open(relative, flags::READ | flags::WRITE | flags::CREATE | flags::TRUNCATE);
    let wrote = if fd >= 0 { write(fd as usize, b"relative") } else { fd };
    if fd >= 0 {
        close(fd as usize);
    }
    let mut short = [0u8; 256];
    let short = path_in(&mut short, dir, "ftest-rel");
    let size = stat(short).map(|stat| stat.size);
    report.check(
        "path-relative-4096",
        relative.len() == abi::PATH_MAX && fd >= 0 && wrote == 8 && size == Ok(8),
        format_args!("{} bytes: open={fd} wrote={wrote} size {size:?}", relative.len()),
    );
    unlink(short);
}

/// Records each `--append` child writes, and their size.
const APPEND_RECORDS: usize = 256;
const APPEND_RECORD: usize = 512;

/// Two processes appending to one file through descriptions of their own at once: every
/// record lands whole, none over another, as `O_APPEND` writes are one step each.
fn test_append(report: &mut Report, program: &str, dir: &str) {
    let mut name = [0u8; 256];
    let path = path_in(&mut name, dir, "ftest-append");
    let fd = open(path, flags::WRITE | flags::CREATE | flags::TRUNCATE);
    if fd < 0 {
        report.check("append-two-processes", false, format_args!("open: {fd}"));
        return;
    }
    close(fd as usize);
    let first = spawn(program, &[program, "--append", path, "--mark", "a"]);
    let second = spawn(program, &[program, "--append", path, "--mark", "b"]);
    let first = if first > 0 { wait(first as usize) } else { first };
    let second = if second > 0 { wait(second as usize) } else { second };
    let size = stat(path).map(|stat| stat.size);
    let mut counts = [0usize; 2];
    let mut torn = 0;
    let fd = open(path, flags::READ);
    if fd >= 0 {
        let mut record = [0u8; APPEND_RECORD];
        while read(fd as usize, &mut record) == APPEND_RECORD as isize {
            match record[0] {
                b'a' if record.iter().all(|&byte| byte == b'a') => counts[0] += 1,
                b'b' if record.iter().all(|&byte| byte == b'b') => counts[1] += 1,
                _ => torn += 1,
            }
        }
        close(fd as usize);
    }
    report.check(
        "append-two-processes",
        first == 0 && second == 0 && size == Ok((2 * APPEND_RECORDS * APPEND_RECORD) as u64) && counts == [APPEND_RECORDS; 2] && torn == 0,
        format_args!("children {first} {second}, size {size:?}, records {counts:?}, {torn} torn"),
    );
    unlink(path);
}

/// `--append PATH --mark C`: appends `APPEND_RECORDS` records of `C` to the file.
fn append(path: &str, mark: u8) -> i32 {
    let fd = open(path, flags::WRITE | flags::APPEND);
    if fd < 0 {
        return 2;
    }
    let record = [mark; APPEND_RECORD];
    for _ in 0..APPEND_RECORDS {
        if write(fd as usize, &record) != APPEND_RECORD as isize {
            return 3;
        }
    }
    close(fd as usize);
    0
}

/// A path or name made with `format_args!`, in a buffer of its own.
struct Text {
    bytes: [u8; 512],
    length: usize,
}

impl Text {
    fn new(args: core::fmt::Arguments<'_>) -> Self {
        let mut text = Self { bytes: [0; 512], length: 0 };
        let _ = text.write_fmt(args);
        text
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl core::fmt::Write for Text {
    fn write_str(&mut self, part: &str) -> core::fmt::Result {
        let end = self.length + part.len();
        if end > self.bytes.len() {
            return Err(core::fmt::Error);
        }
        self.bytes[self.length..end].copy_from_slice(part.as_bytes());
        self.length = end;
        Ok(())
    }
}

macro_rules! text {
    ($($arg:tt)*) => {
        Text::new(format_args!($($arg)*))
    };
}

const CWD: i32 = at::FDCWD;

/// A file made with `data` in it, or the error.
fn make_file(path: &str, data: &[u8]) -> isize {
    let fd = openat(CWD, path, o::WRONLY | o::CREAT | o::TRUNC, 0o644);
    if fd >= 0 {
        write(fd as usize, data);
        close(fd as usize);
        0
    } else {
        fd
    }
}

/// What `FD_PATH` says of `fd`, into `out`.
fn path_of(fd: isize, out: &mut [u8; 512]) -> &str {
    if fd < 0 {
        return "";
    }
    let length = fd_path(fd as usize, out);
    if length < 0 {
        return "";
    }
    core::str::from_utf8(&out[..length as usize]).unwrap_or("")
}

/// The canonical path of `path`, opened `O_PATH`: what a `dirfd` there remembers.
fn canonical(path: &str, out: &mut [u8; 512]) -> usize {
    let fd = openat(CWD, path, o::PATH | o::CLOEXEC, 0);
    let length = path_of(fd, out).len();
    if fd >= 0 {
        close(fd as usize);
    }
    length
}

fn test_openat(report: &mut Report, dir: &str) {
    // Relative to a directory's descriptor, not to the working directory.
    let base = text!("{dir}/ftest-at");
    mkdirat(CWD, base.as_str(), 0o755);
    let dirfd = openat(CWD, base.as_str(), o::RDONLY | o::DIRECTORY, 0);
    let fd = if dirfd >= 0 { openat(dirfd as i32, "inside", o::WRONLY | o::CREAT | o::TRUNC, 0o644) } else { dirfd };
    let wrote = if fd >= 0 { write(fd as usize, b"at") } else { fd };
    if fd >= 0 {
        close(fd as usize);
    }
    let size = fstatat(CWD, text!("{}/inside", base.as_str()).as_str(), 0).map(|stat| stat.size);
    let relative_to_cwd = openat(CWD, "inside", o::RDONLY, 0);
    let by_dirfd = if dirfd >= 0 { fstatat(dirfd as i32, "inside", 0).map(|stat| stat.size) } else { Err(dirfd) };
    report.check(
        "openat-relative-to-dirfd",
        dirfd >= 0 && fd >= 0 && wrote == 2 && size == Ok(2) && by_dirfd == Ok(2) && relative_to_cwd == -errno::ENOENT,
        format_args!("dirfd={dirfd} fd={fd} size {size:?} by the dirfd {by_dirfd:?}, from the cwd {relative_to_cwd}"),
    );

    let path = text!("{}/excl", base.as_str());
    unlink(path.as_str());
    let first = openat(CWD, path.as_str(), o::WRONLY | o::CREAT | o::EXCL, 0o644);
    let again = openat(CWD, path.as_str(), o::WRONLY | o::CREAT | o::EXCL, 0o644);
    let plain = openat(CWD, path.as_str(), o::WRONLY | o::CREAT, 0o644);
    report.check(
        "openat-excl",
        first >= 0 && again == -errno::EEXIST && plain >= 0,
        format_args!("first={first} again={again} without O_EXCL={plain}"),
    );
    for fd in [first, plain] {
        if fd >= 0 {
            close(fd as usize);
        }
    }

    let not_dir = openat(CWD, path.as_str(), o::RDONLY | o::DIRECTORY, 0);
    let missing = openat(CWD, text!("{}/nothing", base.as_str()).as_str(), o::RDONLY, 0);
    let dir_for_writing = openat(CWD, base.as_str(), o::WRONLY, 0);
    // `O_CREAT | O_DIRECTORY` is refused (Linux 6.4's `EINVAL`), and makes nothing.
    let made_dir = text!("{}/creat-dir", base.as_str());
    let create_dir = openat(CWD, made_dir.as_str(), o::RDWR | o::CREAT | o::DIRECTORY, 0o644);
    let left = fstatat(CWD, made_dir.as_str(), at::SYMLINK_NOFOLLOW).map(|stat| stat.mode);
    report.check(
        "openat-directory-enotdir",
        not_dir == -errno::ENOTDIR
            && missing == -errno::ENOENT
            && dir_for_writing == -errno::EISDIR
            && create_dir == -errno::EINVAL
            && left == Err(-errno::ENOENT),
        format_args!(
            "file with O_DIRECTORY={not_dir} missing={missing} directory for writing={dir_for_writing} \
             O_CREAT|O_DIRECTORY={create_dir} left {left:?}"
        ),
    );

    let slash = openat(CWD, text!("{}/", path.as_str()).as_str(), o::RDONLY, 0);
    let dir_slash = openat(CWD, text!("{}/", base.as_str()).as_str(), o::RDONLY, 0);
    let create_slash = openat(CWD, text!("{}/new/", base.as_str()).as_str(), o::WRONLY | o::CREAT, 0o644);
    report.check(
        "openat-trailing-slash-enotdir",
        slash == -errno::ENOTDIR && dir_slash >= 0 && create_slash == -errno::EISDIR,
        format_args!("file/={slash} dir/={dir_slash} new/ with O_CREAT={create_slash}"),
    );
    if dir_slash >= 0 {
        close(dir_slash as usize);
    }

    let made = mkdirat(CWD, text!("{}/sub/", base.as_str()).as_str(), 0o755);
    let kind = fstatat(CWD, text!("{}/sub", base.as_str()).as_str(), 0).map(|stat| stat.mode & s_if::MASK);
    let twice = mkdirat(CWD, text!("{}/sub", base.as_str()).as_str(), 0o755);
    report.check(
        "mkdirat-trailing-slash",
        made == 0 && kind == Ok(s_if::DIR) && twice == -errno::EEXIST,
        format_args!("mkdirat(sub/)={made} kind {kind:?} again={twice}"),
    );

    // The links the boot test made: a file's, and one to a directory.
    let real = text!("{dir}/ftest-real");
    mkdirat(CWD, real.as_str(), 0o755);
    mkdirat(CWD, text!("{}/sub", real.as_str()).as_str(), 0o755);
    make_file(text!("{}/file", real.as_str()).as_str(), b"target");
    let flink = text!("{dir}/ftest-flink");
    let nofollow = openat(CWD, flink.as_str(), o::RDONLY | o::NOFOLLOW, 0);
    let followed = openat(CWD, flink.as_str(), o::RDONLY, 0);
    let link_itself = openat(CWD, flink.as_str(), o::PATH | o::NOFOLLOW, 0);
    let link_kind = if link_itself >= 0 { fstat(link_itself as usize).map(|stat| stat.mode & s_if::MASK) } else { Err(link_itself) };
    report.check(
        "openat-nofollow-eloop",
        nofollow == -errno::ELOOP && followed >= 0 && link_kind == Ok(s_if::LNK),
        format_args!("O_NOFOLLOW={nofollow} followed={followed} O_PATH|O_NOFOLLOW kind {link_kind:?}"),
    );
    for fd in [followed, link_itself] {
        if fd >= 0 {
            close(fd as usize);
        }
    }

    // `..` after a link is the parent of where it led: `ftest-real`, not `dir`.
    let mut expected = [0u8; 512];
    let expected_length = canonical(real.as_str(), &mut expected);
    let expected = core::str::from_utf8(&expected[..expected_length]).unwrap_or("?");
    let up = openat(CWD, text!("{dir}/ftest-hop/..").as_str(), o::RDONLY | o::DIRECTORY, 0);
    let hop = openat(CWD, text!("{dir}/ftest-hop").as_str(), o::RDONLY | o::DIRECTORY, 0);
    let up_from_hop = if hop >= 0 { openat(hop as i32, "..", o::PATH | o::DIRECTORY, 0) } else { hop };
    let mut first = [0u8; 512];
    let mut second = [0u8; 512];
    let (by_path, by_dirfd) = (path_of(up, &mut first), path_of(up_from_hop, &mut second));
    report.check(
        "dotdot-after-symlink-is-physical",
        expected_length > 0 && by_path == expected && by_dirfd == expected,
        format_args!("hop/.. is {by_path}, .. from a dirfd through hop is {by_dirfd}, expected {expected}"),
    );
    for fd in [up, hop, up_from_hop] {
        if fd >= 0 {
            close(fd as usize);
        }
    }

    // `O_PATH`: looks, and does nothing else.
    let target = text!("{}/file", real.as_str());
    let reference = openat(CWD, target.as_str(), o::PATH, 0);
    let looked = if reference >= 0 { fstat(reference as usize) } else { Err(reference) };
    let mut byte = [0u8; 1];
    let read_it = if reference >= 0 { read(reference as usize, &mut byte) } else { reference };
    let flags_of = if reference >= 0 { fcntl(reference as usize, f::GETFL, 0) } else { reference };
    report.check(
        "o-path-fstat",
        looked.map(|stat| (stat.mode & s_if::MASK, stat.size)) == Ok((s_if::REG, 6)) && read_it == -errno::EBADF
            && flags_of == (o::RDONLY | o::PATH) as isize,
        format_args!("fstat {:?} read={read_it} flags={flags_of:#o}", looked.map(|stat| (stat.mode, stat.size))),
    );
    if reference >= 0 {
        close(reference as usize);
    }

    let link = fstatat(CWD, flink.as_str(), at::SYMLINK_NOFOLLOW).map(|stat| (stat.mode & s_if::MASK, stat.size));
    let through = fstatat(CWD, flink.as_str(), 0).map(|stat| (stat.mode & s_if::MASK, stat.size));
    let target_length = target.as_str().len() as i64;
    report.check(
        "fstatat-nofollow",
        link == Ok((s_if::LNK, target_length)) && through == Ok((s_if::REG, 6)),
        format_args!("the link {link:?}, what it leads to {through:?}"),
    );

    // A dangling link is still there to look at, and to refer to: neither follows it.
    unlink(target.as_str());
    let dangling = fstatat(CWD, flink.as_str(), at::SYMLINK_NOFOLLOW).map(|stat| stat.mode & s_if::MASK);
    let followed = fstatat(CWD, flink.as_str(), 0).map(|stat| stat.mode);
    let referred = openat(CWD, flink.as_str(), o::PATH | o::NOFOLLOW, 0);
    report.check(
        "fstatat-nofollow-dangling",
        dangling == Ok(s_if::LNK) && followed == Err(-errno::ENOENT) && referred >= 0,
        format_args!("lstat {dangling:?}, stat {followed:?}, O_PATH|O_NOFOLLOW={referred}"),
    );
    if referred >= 0 {
        close(referred as usize);
    }
    make_file(target.as_str(), b"target");

    let piped = pipe().map(|(reader, writer)| {
        let kinds = (fstat(reader).map(|stat| stat.mode & s_if::MASK), fstat(writer).map(|stat| stat.mode & s_if::MASK));
        close(reader);
        close(writer);
        kinds
    });
    let console = fstat(2).map(|stat| stat.mode & s_if::MASK);
    let fifo = Ok(s_if::FIFO);
    report.check(
        "fstatat-empty-path-pipe",
        piped == Ok((fifo, fifo)) && matches!(console, Ok(s_if::FIFO) | Ok(s_if::CHR)),
        format_args!("pipe ends {piped:?}, standard error {console:?}"),
    );
    if dirfd >= 0 {
        close(dirfd as usize);
    }
}

/// Every name `GETDENTS` gives of the directory `fd`, a `buffer` at a time: a count of
/// them and of names starting `f`, and whether `.` and `..` came first. With `seek_cur`,
/// relibc's `posix_getdents` way: `SEEK_CUR` before each batch, `SEEK_SET` to the last
/// record's cookie after.
fn list_all(fd: usize, buffer: &mut [u8], seek_cur: bool) -> Result<(usize, usize, bool), isize> {
    let (mut total, mut named, mut dots) = (0usize, 0usize, true);
    let mut cookie_seen = 0isize;
    loop {
        if seek_cur {
            let at = seek(fd, 0, whence::CURRENT);
            if at != cookie_seen {
                return Err(at.min(-1));
            }
        }
        let got = getdents(fd, buffer);
        if got < 0 {
            return Err(got);
        }
        if got == 0 {
            return Ok((total, named, dots));
        }
        let mut at = 0;
        let mut last = 0;
        while at < got as usize {
            let Some((header, name)) = Dirent::decode(&buffer[at..got as usize]) else {
                return Err(-1);
            };
            if total == 0 && name != b"." || total == 1 && name != b".." {
                dots = false;
            }
            if name.first() == Some(&b'f') {
                named += 1;
            }
            total += 1;
            last = header.off;
            at += usize::from(header.reclen);
        }
        if seek_cur {
            cookie_seen = seek(fd, last as isize, whence::SET);
        }
    }
}

fn test_getdents(report: &mut Report, dir: &str) {
    let many = text!("{dir}/ftest-list");
    mkdirat(CWD, many.as_str(), 0o755);
    let dirfd = openat(CWD, many.as_str(), o::RDONLY | o::DIRECTORY, 0);
    if dirfd < 0 {
        report.check("getdents-lists-all", false, format_args!("open: {dirfd}"));
        return;
    }
    let dirfd = dirfd as usize;
    let mut made = 0;
    for index in 0..500 {
        let fd = openat(dirfd as i32, text!("f{index:03}").as_str(), o::WRONLY | o::CREAT, 0o644);
        if fd >= 0 {
            made += 1;
            close(fd as usize);
        }
    }
    let mut small = [0u8; 100];
    let listed = list_all(dirfd, &mut small, false);
    report.check(
        "getdents-lists-all",
        made == 500 && listed == Ok((502, 500, true)),
        format_args!("made {made}, listed (all, f*, dots first) {listed:?} through a 100-byte buffer"),
    );

    // A cookie from one batch places the next read there, in this snapshot or a new one.
    seek(dirfd, 0, whence::SET);
    let mut buffer = [0u8; 256];
    let got = getdents(dirfd, &mut buffer);
    let first = Dirent::decode(&buffer[..got.max(0) as usize]);
    let second = first.and_then(|(header, _)| Dirent::decode(&buffer[usize::from(header.reclen)..got.max(0) as usize]));
    let cookie = first.map_or(0, |(header, _)| header.off);
    let mut name = [0u8; 64];
    let name_len = second.map_or(0, |(_, text)| {
        name[..text.len()].copy_from_slice(text);
        text.len()
    });
    getdents(dirfd, &mut buffer);
    let placed = seek(dirfd, cookie as isize, whence::SET);
    let again = getdents(dirfd, &mut buffer);
    let same = Dirent::decode(&buffer[..again.max(0) as usize]).is_some_and(|(_, text)| text == &name[..name_len]);
    // From a new snapshot: rewind, then straight to the cookie.
    seek(dirfd, 0, whence::SET);
    let placed_fresh = seek(dirfd, cookie as isize, whence::SET);
    let fresh = getdents(dirfd, &mut buffer);
    let same_fresh = Dirent::decode(&buffer[..fresh.max(0) as usize]).is_some_and(|(_, text)| text == &name[..name_len]);
    report.check(
        "getdents-seek-cookie",
        got > 0 && name_len > 0 && placed == cookie as isize && same && placed_fresh == cookie as isize && same_fresh,
        format_args!("cookie {cookie} for {:?}: same after seek={same}, after a rewind={same_fresh}",
            core::str::from_utf8(&name[..name_len])),
    );

    seek(dirfd, 0, whence::SET);
    let mut medium = [0u8; 300];
    let paged = list_all(dirfd, &mut medium, true);
    report.check("getdents-seek-cur", paged == Ok((502, 500, true)), format_args!("listed {paged:?}"));

    for index in 0..500 {
        unlinkat(dirfd as i32, text!("f{index:03}").as_str(), 0);
    }
    close(dirfd);
    unlinkat(CWD, many.as_str(), at::REMOVEDIR);

    // A file unlinked while open is an orphan, which no listing shows: not of the root,
    // nor of the directories a mount's root could be.
    let victim = text!("{dir}/ftest-orphan");
    make_file(victim.as_str(), b"orphan");
    let held = openat(CWD, victim.as_str(), o::RDONLY, 0);
    let removed = unlinkat(CWD, victim.as_str(), 0);
    let parent = dir.rsplit_once('/').map_or("/", |(parent, _)| if parent.is_empty() { "/" } else { parent });
    let mut hidden = true;
    let mut listed = 0;
    for place in ["/", parent, dir] {
        let fd = openat(CWD, place, o::RDONLY | o::DIRECTORY, 0);
        if fd < 0 {
            hidden = false;
            continue;
        }
        loop {
            let got = getdents(fd as usize, &mut buffer);
            if got <= 0 {
                break;
            }
            let mut at = 0;
            while let Some((header, name)) = Dirent::decode(&buffer[at..got as usize]) {
                listed += 1;
                if name == b".feos-orphans" || name == b"ftest-orphan" {
                    hidden = false;
                }
                at += usize::from(header.reclen);
            }
        }
        close(fd as usize);
    }
    report.check(
        "getdents-orphans-hidden",
        held >= 0 && removed == 0 && hidden && listed > 0,
        format_args!("held={held} unlinked={removed}, hidden in {listed} entries={hidden}"),
    );
    if held >= 0 {
        close(held as usize);
    }
}

fn test_tree(report: &mut Report, dir: &str) {
    // The umask comes out of a new directory's mode, where the filesystem keeps modes.
    let old = umask(0o077);
    let made_dir = text!("{dir}/ftest-umask");
    let made = mkdirat(CWD, made_dir.as_str(), 0o777);
    let stat = fstatat(CWD, made_dir.as_str(), 0);
    let back = umask(old as usize);
    let posix = stat.is_ok_and(|stat| stat.flags & abi::stat_flags::POSIX != 0);
    let mode = stat.map(|stat| stat.mode & 0o777);
    report.check(
        "mkdirat-umask",
        old == 0o022 && back == 0o077 && made == 0 && (!posix || mode == Ok(0o700)),
        format_args!("umask was {old:#o}, then {back:#o}; made={made} mode {mode:?} (posix={posix})"),
    );
    unlinkat(CWD, made_dir.as_str(), at::REMOVEDIR);

    let full = text!("{dir}/ftest-full");
    mkdirat(CWD, full.as_str(), 0o755);
    let inside = text!("{}/x", full.as_str());
    make_file(inside.as_str(), b"x");
    let not_empty = unlinkat(CWD, full.as_str(), at::REMOVEDIR);
    let file_as_dir = unlinkat(CWD, inside.as_str(), at::REMOVEDIR);
    let dir_as_file = unlinkat(CWD, full.as_str(), 0);
    let dot = unlinkat(CWD, text!("{}/.", full.as_str()).as_str(), at::REMOVEDIR);
    let file_gone = unlinkat(CWD, inside.as_str(), 0);
    let dir_gone = unlinkat(CWD, text!("{}/", full.as_str()).as_str(), at::REMOVEDIR);
    report.check(
        "unlinkat-removedir-enotempty",
        not_empty == -errno::ENOTEMPTY && file_as_dir == -errno::ENOTDIR && dir_as_file == -errno::EISDIR
            && dot == -errno::EINVAL && file_gone == 0 && dir_gone == 0,
        format_args!("not empty={not_empty} file={file_as_dir} dir without the flag={dir_as_file} .={dot} then {file_gone} {dir_gone}"),
    );

    // Unlinked while open: the descriptor still reads and writes it.
    let path = text!("{dir}/ftest-unlinked");
    make_file(path.as_str(), b"still here");
    let fd = openat(CWD, path.as_str(), o::RDWR, 0);
    let removed = unlinkat(CWD, path.as_str(), 0);
    let gone = fstatat(CWD, path.as_str(), 0).err();
    let mut out = [0u8; 16];
    let got = if fd >= 0 { pread(fd as usize, &mut out, 0) } else { fd };
    let wrote = if fd >= 0 { pwrite(fd as usize, b"!", 10) } else { fd };
    let size = if fd >= 0 { fstat(fd as usize).map(|stat| stat.size) } else { Err(fd) };
    let remade = make_file(path.as_str(), b"another");
    let separate = if fd >= 0 { pread(fd as usize, &mut out, 0) } else { fd };
    report.check(
        "unlink-open-file-still-reads",
        fd >= 0 && removed == 0 && gone == Some(-errno::ENOENT) && got == 10 && wrote == 1 && size == Ok(11) && remade == 0
            && separate == 11 && &out[..11] == b"still here!",
        format_args!("unlinked={removed} lookup={gone:?} read={got} write={wrote} size {size:?}, a new file there didn't touch it: {separate}"),
    );
    if fd >= 0 {
        close(fd as usize);
    }
    unlink(path.as_str());

    // Unlinked while mapped, its descriptor closed: the mapping still reads it.
    let path = text!("{dir}/ftest-mapped");
    let mut page = [0u8; PAGE_SIZE];
    for (index, byte) in page.iter_mut().enumerate() {
        *byte = pattern(index);
    }
    make_file(path.as_str(), &page);
    let fd = openat(CWD, path.as_str(), o::RDONLY, 0);
    let address = if fd >= 0 { mmap2_file(0, 2 * PAGE_SIZE, prot::READ, map::PRIVATE, fd as usize, 0) } else { fd };
    if fd >= 0 {
        close(fd as usize);
    }
    let removed = unlinkat(CWD, path.as_str(), 0);
    let same = address > 0 && {
        // SAFETY: just mapped, readable, a page of the file and one past its end.
        let mapped = unsafe { core::slice::from_raw_parts(address as *const u8, PAGE_SIZE) };
        mapped == &page[..]
    };
    if address > 0 {
        munmap(address as usize, 2 * PAGE_SIZE);
    }
    report.check(
        "unlink-mapped-file-still-reads",
        address > 0 && removed == 0 && same,
        format_args!("mapped at {address:#x}, unlinked={removed}, every byte the same={same}"),
    );

    test_running(report, dir);

    let path = text!("{dir}/ftest-trunc");
    make_file(path.as_str(), b"0123456789");
    let fd = openat(CWD, path.as_str(), o::RDWR, 0);
    let grown = if fd >= 0 { ftruncate(fd as usize, 8192) } else { fd };
    let big = fstatat(CWD, path.as_str(), 0).map(|stat| stat.size);
    let mut tail = [0xffu8; 16];
    let tail_read = if fd >= 0 { pread(fd as usize, &mut tail, 8192 - 16) } else { fd };
    let shrunk = if fd >= 0 { ftruncate(fd as usize, 3) } else { fd };
    let small = fstatat(CWD, path.as_str(), 0).map(|stat| stat.size);
    let mut head = [0u8; 8];
    let head_read = if fd >= 0 { pread(fd as usize, &mut head, 0) } else { fd };
    let read_only = openat(CWD, path.as_str(), o::RDONLY, 0);
    let refused = if read_only >= 0 { ftruncate(read_only as usize, 0) } else { read_only };
    let negative = if fd >= 0 { ftruncate(fd as usize, usize::MAX) } else { fd };
    report.check(
        "ftruncate-grow-shrink",
        grown == 0 && big == Ok(8192) && tail_read == 16 && tail == [0; 16] && shrunk == 0 && small == Ok(3) && head_read == 3
            && &head[..3] == b"012" && refused == -errno::EBADF && negative == -errno::EINVAL,
        format_args!("grown={grown} {big:?}, zero tail={}, shrunk={shrunk} {small:?}; read-only={refused} negative={negative}",
            tail == [0; 16]),
    );
    for fd in [fd, read_only] {
        if fd >= 0 {
            close(fd as usize);
        }
    }

    let fd = openat(CWD, path.as_str(), o::RDWR | o::TRUNC, 0);
    if fd >= 0 {
        let fd = fd as usize;
        write(fd, b"abcdefgh");
        seek(fd, 2, whence::SET);
        let put = pwrite(fd, b"XY", 5);
        let mut out = [0u8; 8];
        let got = pread(fd, &mut out, 0);
        let at = seek(fd, 0, whence::CURRENT);
        let next = read(fd, &mut out[..1]);
        let pipe_pread = pipe().map(|(reader, writer)| {
            let result = pread(reader, &mut [0u8; 1], 0);
            close(reader);
            close(writer);
            result
        });
        report.check(
            "pread-pwrite-keep-offset",
            put == 2 && got == 8 && &out[..1] == b"c" && at == 2 && next == 1 && pipe_pread == Ok(-errno::ESPIPE),
            format_args!("pwrite={put} pread={got} offset after {at}, then read {next}; on a pipe {pipe_pread:?}"),
        );
        close(fd);
    }

    let fd = openat(CWD, path.as_str(), o::RDWR | o::APPEND | o::TRUNC, 0);
    if fd >= 0 {
        let fd = fd as usize;
        write(fd, b"0123");
        let put = pwrite(fd, b"x", 1);
        write(fd, b"END");
        let mut out = [0u8; 8];
        let got = pread(fd, &mut out, 0);
        report.check(
            "pwrite-append-at-offset",
            put == 1 && got == 7 && &out[..7] == b"0x23END",
            format_args!("pwrite={put}, the file is {:?}", core::str::from_utf8(&out[..got.max(0) as usize])),
        );
        close(fd);
    }
    unlink(path.as_str());

    test_fsync(report, dir);
}

/// A copy of `/bin/spin`, unlinked while it runs: it finishes as if nothing happened, and
/// its orphan goes with it (the boot test sees the kernel has none left).
fn test_running(report: &mut Report, dir: &str) {
    let copy = text!("{dir}/ftest-spin");
    let source = openat(CWD, "/bin/spin", o::RDONLY, 0);
    let target = openat(CWD, copy.as_str(), o::WRONLY | o::CREAT | o::TRUNC, 0o755);
    let mut copied = 0;
    if source >= 0 && target >= 0 {
        let mut chunk = [0u8; 4096];
        loop {
            let got = read(source as usize, &mut chunk);
            if got <= 0 {
                break;
            }
            copied += write(target as usize, &chunk[..got as usize]).max(0);
        }
    }
    for fd in [source, target] {
        if fd >= 0 {
            close(fd as usize);
        }
    }
    feos_user::chmod(copy.as_str(), 0o755);
    let child = spawn(copy.as_str(), &[copy.as_str(), "5", "1500"]);
    sleep_ms(300);
    let removed = unlinkat(CWD, copy.as_str(), 0);
    let gone = fstatat(CWD, copy.as_str(), 0).err();
    let status = if child > 0 { wait(child as usize) } else { child };
    report.check(
        "unlink-running-program",
        copied > 0 && child > 0 && removed == 0 && gone == Some(-errno::ENOENT) && status == 0,
        format_args!("copied {copied} bytes, pid {child}, unlinked={removed} while it ran, it ended {status}"),
    );
}

/// What a shared mapping dirtied goes to the file at `FSYNC`, and not before: the file's
/// modification time, which only the filesystem's own write changes, says when.
fn test_fsync(report: &mut Report, dir: &str) {
    let path = text!("{dir}/ftest-fsync");
    make_file(path.as_str(), &[b'.'; PAGE_SIZE]);
    let fd = openat(CWD, path.as_str(), o::RDWR, 0);
    if fd < 0 {
        report.check("fsync-writes-back-mapping", false, format_args!("open: {fd}"));
        return;
    }
    let fd = fd as usize;
    let address = mmap2_file(0, PAGE_SIZE, prot::READ | prot::WRITE, map::SHARED, fd, 0);
    let before = fstat(fd).map(|stat| stat.mtime.sec);
    // FAT keeps modification times to 2 s.
    sleep_ms(2_500);
    if address > 0 {
        // SAFETY: just mapped, shared and writable, a page of the file.
        unsafe { core::ptr::write_volatile(address as *mut u8, b'!') };
    }
    let unsynced = fstat(fd).map(|stat| stat.mtime.sec);
    let synced = fsync(fd, 0);
    let data_synced = fsync(fd, abi::FSYNC_DATA);
    let after = fstat(fd).map(|stat| stat.mtime.sec);
    let mut first = [0u8; 1];
    let got = pread(fd, &mut first, 0);
    let bad_flags = fsync(fd, 2);
    let on_pipe = pipe().map(|(reader, writer)| {
        let result = fsync(writer, 0);
        close(reader);
        close(writer);
        result
    });
    let moved = matches!((before, after), (Ok(before), Ok(after)) if after > before);
    report.check(
        "fsync-writes-back-mapping",
        address > 0 && unsynced == before && synced == 0 && data_synced == 0 && moved && got == 1 && &first == b"!"
            && bad_flags == -errno::EINVAL && on_pipe == Ok(-errno::EINVAL),
        format_args!("mtime {before:?}, written through the mapping {unsynced:?}, fsync={synced} then {after:?}; \
            flags 2={bad_flags} a pipe={on_pipe:?}"),
    );
    if address > 0 {
        munmap(address as usize, PAGE_SIZE);
    }
    close(fd);
    unlink(path.as_str());
}

fn test_cwd(report: &mut Report, dir: &str) {
    let mut start = [0u8; 512];
    let start_length = getcwd(&mut start);
    let home = openat(CWD, ".", o::PATH | o::DIRECTORY, 0);
    let target = text!("{dir}/ftest-at");
    let dirfd = openat(CWD, target.as_str(), o::PATH | o::DIRECTORY, 0);
    let changed = if dirfd >= 0 { fchdir(dirfd as usize) } else { dirfd };
    let mut now = [0u8; 512];
    let length = getcwd(&mut now);
    let mut expected = [0u8; 512];
    let expected = path_of(dirfd, &mut expected);
    let here = core::str::from_utf8(&now[..length.max(0) as usize]).unwrap_or("?");
    let relative = fstatat(CWD, "inside", 0).map(|stat| stat.size);
    let file = openat(CWD, "inside", o::RDONLY, 0);
    let not_dir = if file >= 0 { fchdir(file as usize) } else { file };
    let back = if home >= 0 { fchdir(home as usize) } else { home };
    let mut again = [0u8; 512];
    let again_length = getcwd(&mut again);
    report.check(
        "fchdir-getcwd",
        changed == 0 && !expected.is_empty() && here == expected && relative == Ok(2) && not_dir == -errno::ENOTDIR
            && back == 0 && again_length == start_length && again[..again_length.max(0) as usize] == start[..start_length.max(0) as usize],
        format_args!("cwd {here}, relative look {relative:?}, a file={not_dir}, back={back}"),
    );
    // The ERANGE edges need a working directory longer than one byte, and a program's
    // starts at `/`, which fits any buffer but an empty one, so these run inside the
    // test directory.
    let inside = if dirfd >= 0 { fchdir(dirfd as usize) } else { dirfd };
    let mut marked = [0xffu8; 512];
    let length = getcwd(&mut marked);
    let clean = inside == 0 && length > 1 && marked[length as usize] == 0xff && !marked[..length as usize].contains(&0) && marked[0] == b'/';
    report.check("getcwd-no-nul", clean, format_args!("{length} bytes, no NUL, nothing after={clean}"));

    let mut tiny = [0u8; 1];
    let short = getcwd(&mut tiny);
    let none = getcwd(&mut []);
    let exact = if length > 0 { getcwd(&mut marked[..length as usize]) } else { length };
    let one_less = if length > 1 { getcwd(&mut marked[..length as usize - 1]) } else { -errno::ERANGE };
    report.check(
        "getcwd-erange",
        length > 1 && short == -errno::ERANGE && none == -errno::ERANGE && exact == length && one_less == -errno::ERANGE,
        format_args!("one byte={short} none={none} exactly {length}={exact} one short={one_less}"),
    );
    if home >= 0 {
        fchdir(home as usize);
    }
    for fd in [home, dirfd, file] {
        if fd >= 0 {
            close(fd as usize);
        }
    }

    // Whatever way it's named, a descriptor's path is the canonical one.
    let mut base = [0u8; 512];
    let base_length = canonical(dir, &mut base);
    let base = core::str::from_utf8(&base[..base_length]).unwrap_or("?");
    let expected = text!("{base}/ftest-real/sub");
    let roundabout = openat(CWD, text!("{dir}/./ftest-real/../ftest-real//sub/").as_str(), o::RDONLY | o::DIRECTORY, 0);
    let through_link = openat(CWD, text!("{dir}/ftest-hop").as_str(), o::PATH, 0);
    let mut first = [0u8; 512];
    let mut second = [0u8; 512];
    let (by_dots, by_link) = (path_of(roundabout, &mut first), path_of(through_link, &mut second));
    let piped = pipe().map(|(reader, writer)| {
        let result = fd_path(reader, &mut [0u8; 64]);
        close(reader);
        close(writer);
        result
    });
    let mut small = [0u8; 4];
    let short = if roundabout >= 0 { fd_path(roundabout as usize, &mut small) } else { roundabout };
    report.check(
        "fd-path-canonical",
        base_length > 0 && by_dots == expected.as_str() && by_link == expected.as_str() && piped == Ok(-errno::ENOENT)
            && short == -errno::ERANGE,
        format_args!("{by_dots} and {by_link}, expected {}; a pipe {piped:?}, too small {short}", expected.as_str()),
    );
    for fd in [roundabout, through_link] {
        if fd >= 0 {
            close(fd as usize);
        }
    }
}

fn test_exe(report: &mut Report) {
    let mut out = [0u8; 64];
    let length = current_exe(&mut out);
    let exe = core::str::from_utf8(&out[..length.max(0) as usize]).unwrap_or("?");
    let short = current_exe(&mut [0u8; 3]);
    report.check(
        "current-exe",
        exe == "/bin/ftest" && short == -errno::ERANGE,
        format_args!("{exe}, in three bytes {short}"),
    );
}

/// Reads all of a small file into `out`: its length, or the error.
fn slurp(path: &str, out: &mut [u8]) -> isize {
    let fd = openat(CWD, path, o::RDONLY, 0);
    if fd < 0 {
        return fd;
    }
    let mut done = 0;
    while done < out.len() {
        let got = read(fd as usize, &mut out[done..]);
        if got <= 0 {
            break;
        }
        done += got as usize;
    }
    close(fd as usize);
    done as isize
}

/// Whether a stat is of this type, with these permission bits.
fn is(stat: &Result<abi::Stat2, isize>, kind: u32, mode: u32) -> bool {
    stat.is_ok_and(|stat| stat.mode & s_if::MASK == kind && stat.mode & 0o7777 == mode)
}

/// The device nodes of `/dev` (stage 1c.3): `null`, `zero` and `full`, and `/dev/zero`
/// mapped, which is anonymous memory.
fn test_dev(report: &mut Report) {
    let null = openat(CWD, "/dev/null", o::RDWR, 0);
    let mut out = [7u8; 64];
    let got = if null >= 0 { read(null as usize, &mut out) } else { null };
    let stat = fstatat(CWD, "/dev/null", 0);
    let rdev = stat.map(|stat| stat.rdev).unwrap_or(0);
    report.check(
        "dev-null-read-eof",
        got == 0 && is(&stat, s_if::CHR, 0o666) && rdev == 1 << 8 | 3,
        format_args!("read {got}, rdev {rdev:#x}"),
    );
    let wrote = if null >= 0 { write(null as usize, b"discarded") } else { null };
    let again = if null >= 0 { read(null as usize, &mut out) } else { null };
    let size = fstat(null.max(0) as usize).map(|stat| stat.size);
    // As every `> /dev/null` opens it.
    let truncating = openat(CWD, "/dev/null", o::WRONLY | o::TRUNC | o::CREAT, 0o644);
    report.check(
        "dev-null-write-discards",
        wrote == 9 && again == 0 && size == Ok(0) && truncating >= 0,
        format_args!("wrote {wrote}, then read {again}, size {size:?}, O_TRUNC|O_CREAT {truncating}"),
    );
    for fd in [null, truncating] {
        if fd >= 0 {
            close(fd as usize);
        }
    }

    let zero = openat(CWD, "/dev/zero", o::RDWR, 0);
    let mut page = [0xa5u8; 4096];
    let got = if zero >= 0 { read(zero as usize, &mut page) } else { zero };
    let all_zero = page.iter().all(|&byte| byte == 0);
    page.fill(0xa5);
    let far = if zero >= 0 { pread(zero as usize, &mut page[..100], 1 << 40) } else { zero };
    let wrote = if zero >= 0 { write(zero as usize, b"gone") } else { zero };
    report.check(
        "dev-zero-reads-zero",
        got == 4096 && all_zero && far == 100 && page[..100].iter().all(|&byte| byte == 0) && wrote == 4,
        format_args!("read {got} zeroes={all_zero}, far off {far}, wrote {wrote}"),
    );

    // Each mapping of `/dev/zero` is memory of its own, zeroed, private or shared: what
    // one is given isn't in the other, nor in what the device reads.
    let mapped = |flags: usize| {
        if zero < 0 {
            return zero;
        }
        mmap2_file(0, 2 * PAGE_SIZE, prot::READ | prot::WRITE, flags, zero as usize, 0)
    };
    let (private, shared) = (mapped(map::PRIVATE), mapped(map::SHARED));
    let mut good = private > 0 && shared > 0;
    if good {
        // SAFETY: both just mapped, readable and writable, two pages each.
        let (a, b) = unsafe {
            (
                core::slice::from_raw_parts_mut(private as *mut u8, 2 * PAGE_SIZE),
                core::slice::from_raw_parts_mut(shared as *mut u8, 2 * PAGE_SIZE),
            )
        };
        good = a.iter().chain(b.iter()).all(|&byte| byte == 0);
        a[10] = 1;
        b[PAGE_SIZE + 3] = 2;
        good &= b[10] == 0 && a[PAGE_SIZE + 3] == 0;
        page.fill(9);
        good &= read(zero as usize, &mut page[..16]) == 16 && page[..16].iter().all(|&byte| byte == 0);
    }
    for at in [private, shared] {
        if at > 0 {
            munmap(at as usize, 2 * PAGE_SIZE);
        }
    }
    report.check("dev-zero-mmap-is-anon", good, format_args!("private at {private:#x}, shared at {shared:#x}"));
    if zero >= 0 {
        close(zero as usize);
    }

    let full = openat(CWD, "/dev/full", o::RDWR, 0);
    let wrote = if full >= 0 { write(full as usize, b"no room") } else { full };
    page.fill(3);
    let got = if full >= 0 { read(full as usize, &mut page[..32]) } else { full };
    let rdev = fstatat(CWD, "/dev/full", 0).map(|stat| stat.rdev).unwrap_or(0);
    report.check(
        "dev-full-enospc",
        wrote == -errno::ENOSPC && got == 32 && page[..32].iter().all(|&byte| byte == 0) && rdev == 1 << 8 | 7,
        format_args!("wrote {wrote}, read {got}, rdev {rdev:#x}"),
    );
    if full >= 0 {
        close(full as usize);
    }
}

/// `--dev-fd`: writes through `/dev/fd/1` and `/dev/stdout`, which are standard output.
fn dev_fd_child() -> i32 {
    let mut good = true;
    for (path, text) in [("/dev/fd/1", &b"dev-fd-1;"[..]), ("/dev/stdout", &b"dev-stdout;"[..])] {
        let fd = openat(CWD, path, o::WRONLY, 0);
        good &= fd >= 0 && write(fd as usize, text) == text.len() as isize;
        if fd >= 0 {
            close(fd as usize);
        }
    }
    if good { 0 } else { 1 }
}

/// `--console-user`: with a descriptor of the console made as root, as uid 1000 reaches
/// it again through its link and through `/dev/stdout` and `/dev/fd/1` (whatever standard
/// output is), though the `/dev/console` node itself is root's alone. Each descriptor of
/// the console says what `FSTATAT` of `/dev/console` does.
fn console_user_child() -> i32 {
    let console = openat(CWD, "/dev/console", o::WRONLY, 0);
    if console < 0 || feos_user::setuid(1000, 100) != 0 {
        return 2;
    }
    let console = console as usize;
    let mut failed = 0;
    let by_name = openat(CWD, "/dev/console", o::WRONLY, 0);
    failed += usize::from(by_name != -errno::EACCES);
    let node = fstatat(CWD, "/dev/console", 0);
    let same = |a: &abi::Stat2, b: &abi::Stat2| (a.dev, a.ino, a.mode, a.uid, a.rdev) == (b.dev, b.ino, b.mode, b.uid, b.rdev);
    let held = fstat(console);
    failed += usize::from(!matches!((&node, &held), (Ok(node), Ok(held)) if same(node, held) && is(&Ok(*node), s_if::CHR, 0o600)));
    for path in [text!("/proc/self/fd/{console}"), text!("/dev/stdout"), text!("/dev/fd/1")] {
        let fd = openat(CWD, path.as_str(), o::WRONLY, 0);
        if fd < 0 {
            println!("ftest console-user child: {} gave {fd}", path.as_str());
            failed += 1;
            continue;
        }
        // The same description as what it leads to, so it answers as that does.
        let behind = if path.as_str().starts_with("/proc") { console } else { 1 };
        let (got, want) = (fstat(fd as usize), fstat(behind));
        failed += usize::from(!matches!((&got, &want), (Ok(got), Ok(want)) if same(got, want)));
        close(fd as usize);
    }
    println!("ftest console-user child: /dev/console by name {by_name}, {failed} wrong");
    close(console);
    if failed == 0 { 0 } else { 1 }
}

/// `/dev/fd`, `/dev/stdout` and `/dev/shm`.
fn test_dev_links(report: &mut Report, program: &str) {
    // A pipe's end, opened again through its link: the same description.
    let mut shared = false;
    let mut through = 0;
    let mut child_said = -1;
    let mut heard = [0u8; 64];
    let mut length = 0;
    if let Ok((reader, writer)) = pipe() {
        let again = openat(CWD, text!("/dev/fd/{writer}").as_str(), o::WRONLY, 0);
        if again >= 0 {
            through = write(again as usize, b"through;");
            shared = fcntl(again as usize, f::SETFL, o::NONBLOCK) == 0 && fcntl(writer, f::GETFL, 0) as usize & o::NONBLOCK != 0;
            fcntl(writer, f::SETFL, 0);
            close(again as usize);
        }
        // And a child's standard output is this pipe: what it writes to `/dev/fd/1` and
        // `/dev/stdout` comes here.
        let child = spawn_io(program, &[program, "--dev-fd"], abi::INHERIT, writer);
        close(writer);
        child_said = if child > 0 { wait(child as usize) } else { child };
        loop {
            let got = read(reader, &mut heard[length..]);
            if got <= 0 {
                break;
            }
            length += got as usize;
        }
        close(reader);
    }
    let heard = &heard[..length];
    report.check(
        "dev-fd-dups",
        through == 8 && shared && child_said == 0 && heard == b"through;dev-fd-1;dev-stdout;",
        format_args!("wrote {through}, shares flags={shared}, child {child_said}, read {:?}", core::str::from_utf8(heard)),
    );

    let link = fstatat(CWD, "/dev/stdout", at::SYMLINK_NOFOLLOW);
    let size = link.map(|stat| stat.size);
    let behind = fstatat(CWD, "/dev/stdout", 0).map(|stat| stat.mode & s_if::MASK);
    let fd = fstatat(CWD, "/dev/fd", at::SYMLINK_NOFOLLOW);
    let mut seen = [0u8; 512];
    let dir = openat(CWD, "/dev/fd", o::RDONLY | o::DIRECTORY, 0);
    let opened = path_of(dir, &mut seen);
    let mine = text!("/proc/{}/fd", feos_user::getpid());
    let good = is(&link, s_if::LNK, 0o777)
        && size == Ok("/proc/self/fd/1".len() as i64)
        && matches!(behind, Ok(s_if::CHR | s_if::FIFO))
        && is(&fd, s_if::LNK, 0o777)
        && opened == mine.as_str();
    report.check("dev-stdout-link", good, format_args!("link size {size:?}, leads to type {:o}, /dev/fd is {opened}", behind.unwrap_or(0)));
    if dir >= 0 {
        close(dir as usize);
    }

    let child = spawn(program, &[program, "--console-user"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    report.check("dev-console-as-user", status == 0, format_args!("the child said {status}"));

    // A file in `/dev/shm` mapped shared and writable: a reopen reads what was written.
    let shm = text!("/dev/shm/ftest-{}", feos_user::getpid());
    let directory = fstatat(CWD, "/dev/shm", 0);
    let fd = openat(CWD, shm.as_str(), o::RDWR | o::CREAT | o::EXCL, 0o600);
    let mut wrote = false;
    if fd >= 0 && ftruncate(fd as usize, PAGE_SIZE) == 0 {
        let at = mmap2_file(0, PAGE_SIZE, prot::READ | prot::WRITE, map::SHARED, fd as usize, 0);
        if at > 0 {
            // SAFETY: just mapped, readable and writable, one page.
            let memory = unsafe { core::slice::from_raw_parts_mut(at as *mut u8, PAGE_SIZE) };
            memory[..6].copy_from_slice(b"shared");
            memory[PAGE_SIZE - 1] = b'!';
            wrote = munmap(at as usize, PAGE_SIZE) == 0;
        }
    }
    if fd >= 0 {
        close(fd as usize);
    }
    let mut back = [0u8; PAGE_SIZE];
    let got = slurp(shm.as_str(), &mut back);
    let removed = unlinkat(CWD, shm.as_str(), 0);
    report.check(
        "dev-shm-map-shared",
        is(&directory, s_if::DIR, 0o1777) && wrote && got == PAGE_SIZE as isize && &back[..6] == b"shared" && back[PAGE_SIZE - 1] == b'!' && removed == 0,
        format_args!("made {fd}, mapped and written={wrote}, read back {got}, removed {removed}"),
    );
}

/// `/proc/self`: the program, its descriptors, and nobody else's.
fn test_proc(report: &mut Report, dir: &str) {
    let followed = fstatat(CWD, "/proc/self/exe", 0);
    let link = fstatat(CWD, "/proc/self/exe", at::SYMLINK_NOFOLLOW);
    let v1 = stat("/proc/self/exe");
    let executable = followed.is_ok_and(|stat| stat.mode & s_if::MASK == s_if::REG && stat.size > 0);
    report.check(
        "proc-self-exe-exists",
        executable && is(&link, s_if::LNK, 0o777) && v1.is_ok(),
        format_args!("followed {:?}, link {:?}, v1 stat ok={}", followed.map(|stat| stat.mode), link.map(|stat| stat.mode), v1.is_ok()),
    );

    let mut exe = [0u8; 512];
    let length = current_exe(&mut exe).max(0) as usize;
    let exe = core::str::from_utf8(&exe[..length]).unwrap_or("?");
    let fd = openat(CWD, "/proc/self/exe", o::RDONLY | o::CLOEXEC, 0);
    let mut out = [0u8; 512];
    let opened = path_of(fd, &mut out);
    let size = link.map(|stat| stat.size as usize);
    report.check(
        "proc-self-exe-matches-current-exe",
        !exe.is_empty() && opened == exe && size == Ok(exe.len()),
        format_args!("CURRENT_EXE {exe}, opened {opened}, link size {size:?}"),
    );
    if fd >= 0 {
        close(fd as usize);
    }

    // Three files open, and the listing of `/proc/self/fd` has each, linked to where
    // `FD_PATH` says it is.
    let mut files = [-1isize; 3];
    for (index, fd) in files.iter_mut().enumerate() {
        let path = text!("{dir}/ftest-proc-{index}");
        make_file(path.as_str(), b"x");
        *fd = openat(CWD, path.as_str(), o::RDONLY, 0);
    }
    let listing = openat(CWD, "/proc/self/fd", o::RDONLY | o::DIRECTORY, 0);
    let mut listed = [false; 1024];
    let mut stray = 0;
    let mut buffer = [0u8; 2048];
    loop {
        let got = if listing >= 0 { getdents(listing as usize, &mut buffer) } else { listing };
        if got <= 0 {
            break;
        }
        let mut at = 0;
        while at < got as usize {
            let Some((header, name)) = Dirent::decode(&buffer[at..got as usize]) else {
                break;
            };
            at += usize::from(header.reclen);
            if name == b"." || name == b".." {
                continue;
            }
            match core::str::from_utf8(name).ok().and_then(parse_number) {
                // Every one listed is open.
                Some(number) if number < listed.len() && fcntl(number, f::GETFD, 0) >= 0 && header.kind == abi::dt::LNK => {
                    listed[number] = true;
                }
                _ => stray += 1,
            }
        }
    }
    let mut links_match = true;
    for &fd in files.iter().chain([listing].iter()) {
        if fd < 0 || !listed[fd as usize] {
            links_match = false;
            continue;
        }
        let mut wanted = [0u8; 512];
        let wanted = path_of(fd, &mut wanted);
        let entry = text!("/proc/self/fd/{fd}");
        let through = openat(CWD, entry.as_str(), o::PATH | o::CLOEXEC, 0);
        let mut found = [0u8; 512];
        let found = path_of(through, &mut found);
        let size = fstatat(CWD, entry.as_str(), at::SYMLINK_NOFOLLOW).map(|stat| stat.size as usize);
        links_match &= !wanted.is_empty() && found == wanted && size == Ok(wanted.len());
        if through >= 0 {
            close(through as usize);
        }
    }
    let streams = listed[0] && listed[1] && listed[2];
    report.check(
        "proc-self-fd-lists-open",
        streams && links_match && stray == 0,
        format_args!("files {files:?} listing {listing}: streams={streams} links match={links_match}, stray {stray}"),
    );
    for (index, fd) in files.iter().enumerate() {
        if *fd >= 0 {
            close(*fd as usize);
        }
        unlinkat(CWD, text!("{dir}/ftest-proc-{index}").as_str(), 0);
    }
    if listing >= 0 {
        close(listing as usize);
    }
    let closed = fstatat(CWD, text!("/proc/self/fd/{}", files[0]).as_str(), at::SYMLINK_NOFOLLOW).err();

    let own = fstatat(CWD, text!("/proc/{}", feos_user::getpid()).as_str(), 0);
    let other = fstatat(CWD, "/proc/1", 0).err();
    let other_exe = openat(CWD, "/proc/1/exe", o::RDONLY, 0);
    report.check(
        "proc-other-pid-enoent",
        is(&own, s_if::DIR, 0o555) && other == Some(-errno::ENOENT) && other_exe == -errno::ENOENT && closed == Some(-errno::ENOENT),
        format_args!("own dir ok={}, /proc/1 {other:?}, its exe {other_exe}, a closed fd's {closed:?}", own.is_ok()),
    );
}

/// `--sticky PATH`: as a user other than root, fails to remove root's `PATH` from `/tmp`
/// (`EPERM`), and can make and remove a file of its own there.
fn sticky_child(path: &str) -> i32 {
    if feos_user::setuid(1000, 100) != 0 {
        return 2;
    }
    let refused = unlinkat(CWD, path, 0);
    let v1 = unlink(path);
    let own = text!("{path}-own");
    let made = make_file(own.as_str(), b"mine");
    let owner = fstatat(CWD, own.as_str(), 0).map(|stat| stat.uid);
    let removed = unlinkat(CWD, own.as_str(), 0);
    println!("ftest sticky child: unlinkat {refused}, v1 unlink {v1}, own made {made} by {owner:?} removed {removed}");
    if refused == -errno::EPERM && v1 == -errno::EPERM && made == 0 && owner == Ok(1000) && removed == 0 { 0 } else { 1 }
}

/// `/tmp` is sticky: a user that isn't root can't remove root's file there.
fn test_sticky(report: &mut Report, program: &str) {
    let tmp = fstatat(CWD, "/tmp", 0);
    let path = text!("/tmp/ftest-sticky-{}", feos_user::getpid());
    let made = make_file(path.as_str(), b"root's");
    let child = spawn(program, &[program, "--sticky", path.as_str()]);
    let status = if child > 0 { wait(child as usize) } else { child };
    let still = fstatat(CWD, path.as_str(), 0).is_ok();
    let removed = unlinkat(CWD, path.as_str(), 0);
    report.check(
        "tmp-sticky",
        is(&tmp, s_if::DIR, 0o1777) && made == 0 && status == 0 && still && removed == 0,
        format_args!(
            "/tmp mode {:o}, the child said {status}, root's file still there={still}, then removed {removed}",
            tmp.map_or(0, |stat| stat.mode)
        ),
    );
}

/// The image's layout (section 10.4 of the ABI design): `/etc`'s files parse, and the
/// directories and links a POSIX program looks for are there.
fn test_etc(report: &mut Report) {
    let mut buffer = [0u8; 4096];
    let fields = |text: &str, count: usize, first: &str, number: &str| {
        let lines = text.lines().filter(|line| !line.is_empty() && !line.starts_with('#'));
        let mut found = false;
        for line in lines {
            let parts: [Option<&str>; 8] = {
                let mut parts = [None; 8];
                for (slot, part) in parts.iter_mut().zip(line.split(':')) {
                    *slot = Some(part);
                }
                parts
            };
            if parts.iter().filter(|part| part.is_some()).count() != count || line.split(':').count() != count {
                return false;
            }
            found |= parts[0] == Some(first) && parts[2] == Some(number);
        }
        found
    };
    let got = slurp("/etc/passwd", &mut buffer);
    let passwd = got > 0 && fields(core::str::from_utf8(&buffer[..got as usize]).unwrap_or(""), 7, "root", "0");
    let got = slurp("/etc/group", &mut buffer);
    let group = got > 0 && fields(core::str::from_utf8(&buffer[..got as usize]).unwrap_or(""), 4, "root", "0");
    let got = slurp("/etc/hosts", &mut buffer);
    let hosts = got > 0
        && core::str::from_utf8(&buffer[..got as usize]).unwrap_or("").lines().any(|line| {
            let mut words = line.split_whitespace();
            words.next() == Some("127.0.0.1") && words.any(|word| word == "localhost")
        });
    let got = slurp("/etc/localtime", &mut buffer);
    let localtime = got > 4 && buffer.starts_with(b"TZif");
    let others = ["/etc/services", "/etc/hostname", "/etc/shells"].iter().all(|path| fstatat(CWD, path, 0).is_ok_and(|stat| stat.size > 0));
    // The CA bundle is the build machine's (the container's), not the tree's: an image
    // built without one would leave rustls nothing to trust.
    let got = slurp("/etc/ssl/certs/ca-certificates.crt", &mut buffer);
    // Some machines' bundles open with comments before the first certificate.
    let begin = b"-----BEGIN CERTIFICATE-----";
    let certificates = got > 0 && buffer[..got as usize].windows(begin.len()).any(|window| window == begin);
    report.check(
        "etc-files-present",
        passwd && group && hosts && localtime && others && certificates,
        format_args!(
            "passwd={passwd} group={group} hosts={hosts} localtime={localtime} services, hostname, shells={others} CA bundle={certificates}"
        ),
    );

    let dirs = ["/root", "/home", "/home/user", "/usr/lib", "/usr/include", "/bin"]
        .iter()
        .all(|path| fstatat(CWD, path, 0).is_ok_and(|stat| stat.mode & s_if::MASK == s_if::DIR));
    let mut out = [0u8; 512];
    let usr_bin = openat(CWD, "/usr/bin", o::PATH | o::DIRECTORY, 0);
    let leads = path_of(usr_bin, &mut out) == "/bin";
    let link = fstatat(CWD, "/usr/bin", at::SYMLINK_NOFOLLOW).is_ok_and(|stat| stat.mode & s_if::MASK == s_if::LNK);
    let through = fstatat(CWD, "/usr/bin/ftest", 0).is_ok();
    report.check("image-layout", dirs && leads && link && through, format_args!("dirs={dirs} /usr/bin a link={link} to /bin={leads}, ftest there={through}"));
    if usr_bin >= 0 {
        close(usr_bin as usize);
    }
}


/// What the filesystem under test keeps, as the boot test says: `--coarse` for FAT (two
/// second times, no owners or modes, no hard links), `--hard-links` for the native format.
#[derive(Clone, Copy)]
struct Keeps {
    coarse: bool,
    hard_links: bool,
}

/// A copy of `/bin/spin` at `path`, runnable, and how many bytes it is.
fn copy_spin(path: &str) -> isize {
    let source = openat(CWD, "/bin/spin", o::RDONLY, 0);
    let target = openat(CWD, path, o::WRONLY | o::CREAT | o::TRUNC, 0o755);
    let mut copied = 0;
    if source >= 0 && target >= 0 {
        let mut chunk = [0u8; 4096];
        loop {
            let got = read(source as usize, &mut chunk);
            if got <= 0 {
                break;
            }
            copied += write(target as usize, &chunk[..got as usize]).max(0);
        }
    }
    for fd in [source, target] {
        if fd >= 0 {
            close(fd as usize);
        }
    }
    feos_user::chmod(path, 0o755);
    if source < 0 { source } else if target < 0 { target } else { copied }
}

/// Renames (stage 1c.4): over what's there in one step, or not with `RENAME_NOREPLACE`;
/// never across mounts nor under itself; and what's replaced keeps working for whoever
/// has it open, or is running it.
fn test_rename(report: &mut Report, dir: &str) {
    let (from, to) = (text!("{dir}/ftest-mv-from"), text!("{dir}/ftest-mv-to"));
    make_file(from.as_str(), b"new contents");
    make_file(to.as_str(), b"old");
    let moving = fstatat(CWD, from.as_str(), 0).map(|stat| stat.ino);
    let renamed = renameat(CWD, from.as_str(), CWD, to.as_str(), 0);
    let mut out = [0u8; 32];
    let got = slurp(to.as_str(), &mut out);
    let after = fstatat(CWD, to.as_str(), 0).map(|stat| stat.ino);
    let gone = fstatat(CWD, from.as_str(), 0).err();
    report.check(
        "rename-over-existing",
        renamed == 0 && &out[..got.max(0) as usize] == b"new contents" && after == moving && gone == Some(-errno::ENOENT),
        format_args!("renamed {renamed}, reads {got} bytes, ino {moving:?} then {after:?}, old name {gone:?}"),
    );

    make_file(from.as_str(), b"again");
    let refused = renameat(CWD, from.as_str(), CWD, to.as_str(), rename::NOREPLACE);
    let kept = slurp(to.as_str(), &mut out);
    let still = fstatat(CWD, from.as_str(), 0).is_ok();
    report.check(
        "rename-noreplace-eexist",
        refused == -errno::EEXIST && &out[..kept.max(0) as usize] == b"new contents" && still,
        format_args!("{} with the old file kept={still}", feos_user::error_name(refused)),
    );

    // Another mount: the root's RAM, or /tmp's, whichever the directory isn't on.
    let elsewhere = if dir.starts_with("/tmp") { "/ftest-xdev" } else { "/tmp/ftest-xdev" };
    let across = renameat(CWD, from.as_str(), CWD, elsewhere, 0);
    let left = fstatat(CWD, from.as_str(), 0).is_ok();
    report.check(
        "rename-exdev",
        across == -errno::EXDEV && left && fstatat(CWD, elsewhere, 0).is_err(),
        format_args!("to {elsewhere}: {}, still at the old name={left}", feos_user::error_name(across)),
    );
    unlinkat(CWD, from.as_str(), 0);

    let tree = text!("{dir}/ftest-mv-dir");
    mkdirat(CWD, tree.as_str(), 0o755);
    let into = renameat(CWD, tree.as_str(), CWD, text!("{}/sub", tree.as_str()).as_str(), 0);
    let empty = text!("{dir}/ftest-mv-empty");
    mkdirat(CWD, empty.as_str(), 0o755);
    let over_empty = renameat(CWD, tree.as_str(), CWD, empty.as_str(), 0);
    let moved = fstatat(CWD, empty.as_str(), 0).map(|stat| stat.mode & s_if::MASK);
    let slash = renameat(CWD, to.as_str(), CWD, text!("{dir}/ftest-mv-slash/").as_str(), 0);
    report.check(
        "rename-dir-into-itself-einval",
        into == -errno::EINVAL && over_empty == 0 && moved == Ok(s_if::DIR) && slash == -errno::ENOTDIR,
        format_args!("under itself {}, over an empty one {over_empty}, a file to a slash {}", feos_user::error_name(into), feos_user::error_name(slash)),
    );
    unlinkat(CWD, empty.as_str(), at::REMOVEDIR);

    // Open, then replaced: the description still reads the old file.
    make_file(from.as_str(), b"replacement");
    let open = openat(CWD, to.as_str(), o::RDONLY, 0);
    let replaced = renameat(CWD, from.as_str(), CWD, to.as_str(), 0);
    let mut old = [0u8; 32];
    let through = if open >= 0 { pread(open as usize, &mut old, 0) } else { open };
    let now = slurp(to.as_str(), &mut out);
    if open >= 0 {
        close(open as usize);
    }
    report.check(
        "rename-over-open-file-still-reads",
        replaced == 0 && &old[..through.max(0) as usize] == b"new contents" && &out[..now.max(0) as usize] == b"replacement",
        format_args!("renamed {replaced}; the open one reads {through} bytes, the name {now}"),
    );
    unlinkat(CWD, to.as_str(), 0);

    // `tempfile`'s persist: a new copy written beside a running program, renamed over it.
    let running = text!("{dir}/ftest-run");
    let temporary = text!("{dir}/.ftest-run.tmp");
    let copied = copy_spin(running.as_str());
    let child = spawn(running.as_str(), &[running.as_str(), "7", "1500"]);
    sleep_ms(300);
    let second = copy_spin(temporary.as_str());
    let persisted = renameat(CWD, temporary.as_str(), CWD, running.as_str(), 0);
    let size = fstatat(CWD, running.as_str(), 0).map(|stat| stat.size);
    let status = if child > 0 { wait(child as usize) } else { child };
    unlinkat(CWD, running.as_str(), 0);
    report.check(
        "rename-over-running-program",
        copied > 0 && child > 0 && second == copied && persisted == 0 && size == Ok(copied as i64) && status == 0,
        format_args!("copied {copied} and {second} bytes, pid {child}, renamed over it {persisted} while it ran, it ended {status}"),
    );
}

/// Links (stage 1c.4): hard links following a link or not, symbolic links and reading
/// them back, cut short to the buffer.
fn test_links(report: &mut Report, dir: &str, keeps: Keeps) {
    let (file, link) = (text!("{dir}/ftest-ln-file"), text!("{dir}/ftest-ln-sym"));
    let (hard, own) = (text!("{dir}/ftest-ln-hard"), text!("{dir}/ftest-ln-itself"));
    make_file(file.as_str(), b"linked");
    let target = "ftest-ln-file";
    let made = symlinkat(target, CWD, link.as_str());
    let again = symlinkat(target, CWD, link.as_str());
    let mut out = [0u8; 64];
    let length = readlinkat(CWD, link.as_str(), &mut out);
    let lstat = fstatat(CWD, link.as_str(), at::SYMLINK_NOFOLLOW);
    let through = slurp(link.as_str(), &mut [0u8; 16]);
    report.check(
        "symlinkat-readlinkat",
        made == 0
            && again == -errno::EEXIST
            && &out[..length.max(0) as usize] == target.as_bytes()
            && lstat.is_ok_and(|stat| stat.mode & s_if::MASK == s_if::LNK && stat.size == target.len() as i64)
            && through == 6,
        format_args!("made {made}, again {}, read back {length} bytes, reads through it {through}", feos_user::error_name(again)),
    );
    let mut short = [0u8; 4];
    let cut = readlinkat(CWD, link.as_str(), &mut short);
    let not_link = readlinkat(CWD, file.as_str(), &mut out);
    let zero = readlinkat(CWD, link.as_str(), &mut []);
    report.check(
        "readlink-truncates",
        cut == 4 && &short == b"ftes" && not_link == -errno::EINVAL && zero == -errno::EINVAL,
        format_args!("into 4 bytes {cut}, of a file {}, into none {}", feos_user::error_name(not_link), feos_user::error_name(zero)),
    );

    let followed = linkat(CWD, link.as_str(), CWD, hard.as_str(), at::SYMLINK_FOLLOW);
    let itself = linkat(CWD, link.as_str(), CWD, own.as_str(), 0);
    let file_ino = fstatat(CWD, file.as_str(), 0).map(|stat| (stat.ino, stat.nlink));
    let hard_ino = fstatat(CWD, hard.as_str(), at::SYMLINK_NOFOLLOW).map(|stat| (stat.ino, stat.mode & s_if::MASK));
    let own_kind = fstatat(CWD, own.as_str(), at::SYMLINK_NOFOLLOW).map(|stat| stat.mode & s_if::MASK);
    let good = if keeps.hard_links {
        followed == 0
            && itself == 0
            && file_ino.is_ok_and(|(_, links)| links == 2)
            && hard_ino.is_ok_and(|(ino, kind)| Ok(ino) == file_ino.map(|(ino, _)| ino) && kind == s_if::REG)
            && own_kind == Ok(s_if::LNK)
    } else {
        followed == -errno::EOPNOTSUPP && itself == -errno::EOPNOTSUPP
    };
    report.check(
        "linkat-follow",
        good,
        format_args!("following {}, the link itself {}, file {file_ino:?}, new name {hard_ino:?}", feos_user::error_name(followed), feos_user::error_name(itself)),
    );
    for name in [&hard, &own, &link, &file] {
        unlinkat(CWD, name.as_str(), 0);
    }
}

/// A time, in nanoseconds since the epoch.
fn nanos(time: abi::Timespec) -> u64 {
    time.sec as u64 * 1_000_000_000 + time.nsec as u64
}

/// Modes, owners and times (stage 1c.4), through paths and through descriptors.
fn test_attributes(report: &mut Report, program: &str, dir: &str, keeps: Keeps) {
    let file = text!("{dir}/ftest-attr");
    make_file(file.as_str(), b"attributes");
    let fd = openat(CWD, file.as_str(), o::RDONLY, 0);
    let fd = fd.max(0) as usize;
    let changed = fchmodat(fd as i32, "", 0o600, at::EMPTY_PATH);
    let mode = fstat(fd).map(|stat| stat.mode & 0o7777);
    let nofollow = fchmodat(CWD, file.as_str(), 0o600, at::SYMLINK_NOFOLLOW);
    let good = if keeps.coarse { changed == -errno::EOPNOTSUPP } else { changed == 0 && mode == Ok(0o600) };
    report.check(
        "fchmodat-empty-path",
        good && nofollow == -errno::EOPNOTSUPP,
        format_args!("through the descriptor {}, mode now {mode:?}, not following {}", feos_user::error_name(changed), feos_user::error_name(nofollow)),
    );

    // Root gives a file away; anyone else may only leave both alone.
    let given = fchownat(CWD, file.as_str(), 1000, 100, 0);
    let owner = fstatat(CWD, file.as_str(), 0).map(|stat| (stat.uid, stat.gid));
    let child = spawn(program, &[program, "--chown-user", file.as_str()]);
    let status = if child > 0 { wait(child as usize) } else { child };
    let good = if keeps.coarse { given == -errno::EOPNOTSUPP } else { given == 0 && owner == Ok((1000, 100)) };
    report.check(
        "fchownat-root-only",
        good && status == 0,
        format_args!("as root {}, now {owner:?}; as 1000 the child said {status}", feos_user::error_name(given)),
    );
    // `-1` either way it's written: a C caller's sign-extended, a Rust caller's `u32::MAX`.
    let group_only = fchownat(CWD, file.as_str(), abi::KEEP, 7, 0);
    let user_only = fchownat(CWD, file.as_str(), 8, u32::MAX as usize, 0);
    let both = fchownat(CWD, file.as_str(), u32::MAX as usize, abi::KEEP, 0);
    let now = fstatat(CWD, file.as_str(), 0).map(|stat| (stat.uid, stat.gid));
    let good = if keeps.coarse { both == 0 } else { group_only == 0 && user_only == 0 && both == 0 && now == Ok((8, 7)) };
    report.check(
        "fchownat-keep-both-widths",
        good,
        format_args!("group alone {group_only}, owner alone {user_only}, neither {both}: {owner:?} to {now:?}"),
    );
    fchownat(CWD, file.as_str(), 0, 0, 0);

    // Set, then one left and one now.
    let base = abi::Timespec { sec: 1_600_000_000, nsec: 0 };
    let set = utimensat(CWD, file.as_str(), Some(&[base, abi::Timespec { sec: 1_600_000_100, nsec: 0 }]), 0);
    let before = fstatat(CWD, file.as_str(), 0);
    let clock = feos_user::now_ns(abi::clock::REALTIME);
    let omitted = utimensat(
        CWD,
        file.as_str(),
        Some(&[abi::Timespec { sec: 0, nsec: abi::utime::OMIT }, abi::Timespec { sec: 0, nsec: abi::utime::NOW }]),
        0,
    );
    let after = fstatat(CWD, file.as_str(), 0);
    // Two seconds' slack for FAT's steps.
    let good = before.ok().zip(after.ok()).is_some_and(|(before, after)| {
        after.atime == before.atime && nanos(after.mtime) + 2_000_000_000 >= clock && nanos(before.mtime) < clock
    });
    report.check(
        "utimensat-now-omit",
        set == 0 && omitted == 0 && good,
        format_args!(
            "set {set}, then {omitted}: atime {:?} to {:?}, mtime {:?} to {:?}",
            before.map(|stat| stat.atime.sec),
            after.map(|stat| stat.atime.sec),
            before.map(|stat| stat.mtime.sec),
            after.map(|stat| stat.mtime.sec)
        ),
    );
    let fine = abi::Timespec { sec: 1_600_000_001, nsec: 123_456_789 };
    let through_fd = utimensat(fd as i32, "", Some(&[fine, fine]), at::EMPTY_PATH);
    let kept = fstatat(CWD, file.as_str(), 0).map(|stat| stat.mtime);
    // FAT keeps the two-second step below it.
    let wanted = if keeps.coarse { abi::Timespec { sec: 1_600_000_000, nsec: 0 } } else { fine };
    let bad = utimensat(CWD, file.as_str(), Some(&[abi::Timespec { sec: 0, nsec: 1_000_000_000 }, fine]), 0);
    report.check(
        "utimensat-nanoseconds",
        through_fd == 0 && kept == Ok(wanted) && bad == -errno::EINVAL,
        format_args!("set {through_fd} through the descriptor, kept {kept:?}, a bad nsec {}", feos_user::error_name(bad)),
    );
    close(fd);

    // Two writes 10 ms apart: the second is later, where times are finer than that.
    let first = make_file(file.as_str(), b"one");
    let one = fstatat(CWD, file.as_str(), 0).map(|stat| nanos(stat.mtime));
    sleep_ms(10);
    let second = make_file(file.as_str(), b"two");
    let two = fstatat(CWD, file.as_str(), 0).map(|stat| nanos(stat.mtime));
    let good = keeps.coarse || one.ok().zip(two.ok()).is_some_and(|(one, two)| two > one);
    report.check(
        "mtime-advances-within-second",
        first == 0 && second == 0 && good,
        format_args!("{one:?} then {two:?}{}", if keeps.coarse { " (two-second steps here)" } else { "" }),
    );

    // A chmod is a change of status, not of contents.
    let before = fstatat(CWD, file.as_str(), 0);
    sleep_ms(10);
    let chmod = fchmodat(CWD, file.as_str(), 0o640, 0);
    let after = fstatat(CWD, file.as_str(), 0);
    let good = if keeps.coarse {
        chmod == -errno::EOPNOTSUPP
    } else {
        chmod == 0 && before.ok().zip(after.ok()).is_some_and(|(before, after)| nanos(after.ctime) > nanos(before.ctime) && after.mtime == before.mtime)
    };
    report.check(
        "ctime-on-chmod",
        good,
        format_args!("chmod {}, ctime {:?} to {:?}", feos_user::error_name(chmod), before.map(|stat| nanos(stat.ctime)), after.map(|stat| nanos(stat.ctime))),
    );

    // Making an entry changes the directory.
    let directory = text!("{dir}/ftest-dir-time");
    mkdirat(CWD, directory.as_str(), 0o755);
    let aged = utimensat(CWD, directory.as_str(), Some(&[base, base]), 0);
    let made = make_file(text!("{}/new", directory.as_str()).as_str(), b"n");
    let changed = fstatat(CWD, directory.as_str(), 0).map(|stat| stat.mtime.sec);
    report.check(
        "dir-mtime-on-create",
        aged == 0 && made == 0 && changed.is_ok_and(|sec| sec > base.sec),
        format_args!("aged {aged}, made a file {made}: the directory's mtime {} then {changed:?}", base.sec),
    );
    unlinkat(CWD, text!("{}/new", directory.as_str()).as_str(), 0);
    unlinkat(CWD, directory.as_str(), at::REMOVEDIR);

    // What may be done, as root: anything but run a file nobody may.
    fchmodat(CWD, file.as_str(), 0o644, 0);
    let exists = faccessat(CWD, file.as_str(), abi::access::F_OK, 0);
    let readable = faccessat(CWD, file.as_str(), abi::access::R_OK | abi::access::W_OK, abi::at::EACCESS);
    let runnable = faccessat(CWD, file.as_str(), abi::access::X_OK, 0);
    let missing = faccessat(CWD, text!("{dir}/ftest-not-there").as_str(), abi::access::F_OK, 0);
    let bad = faccessat(CWD, file.as_str(), 8, 0);
    let fd = openat(CWD, file.as_str(), o::PATH, 0);
    let through_fd = faccessat(fd as i32, "", abi::access::R_OK, at::EMPTY_PATH);
    if fd >= 0 {
        close(fd as usize);
    }
    let run_wanted = if keeps.coarse { 0 } else { -errno::EACCES };
    report.check(
        "faccessat-modes",
        exists == 0 && readable == 0 && runnable == run_wanted && missing == -errno::ENOENT && bad == -errno::EINVAL && through_fd == 0,
        format_args!(
            "F_OK {exists}, R_OK|W_OK {readable}, X_OK of 0644 {}, missing {}, mode 8 {}, through O_PATH {through_fd}",
            feos_user::error_name(runnable),
            feos_user::error_name(missing),
            feos_user::error_name(bad)
        ),
    );
    unlinkat(CWD, file.as_str(), 0);
}

/// `--chown-user PATH`: as a user other than root, `FCHOWNAT` gives nothing away
/// (`EPERM`), but leaving both alone succeeds.
fn chown_child(path: &str) -> i32 {
    if feos_user::setuid(1000, 100) != 0 {
        return 2;
    }
    let refused = fchownat(CWD, path, 1000, 100, 0);
    let kept = fchownat(CWD, path, abi::KEEP, u32::MAX as usize, 0);
    println!("ftest chown child: giving {refused}, leaving both {kept}");
    if refused == -errno::EPERM && kept == 0 { 0 } else { 1 }
}

/// The filesystem's room (stage 1c.4), and `SYNC`.
fn test_statvfs(report: &mut Report, dir: &str) {
    let directory = openat(CWD, dir, o::RDONLY | o::DIRECTORY, 0);
    let before = if directory >= 0 { fstatvfs(directory as usize) } else { Err(directory) };
    let dev = if directory >= 0 { fstat(directory as usize).map(|stat| stat.dev) } else { Err(directory) };
    let file = text!("{dir}/ftest-room");
    let fd = openat(CWD, file.as_str(), o::RDWR | o::CREAT | o::TRUNC, 0o644);
    let chunk = [0x5au8; 4096];
    for _ in 0..64 {
        write(fd.max(0) as usize, &chunk);
    }
    fsync(fd.max(0) as usize, 0);
    let on_file = if fd >= 0 { fstatvfs(fd as usize) } else { Err(fd) };
    if fd >= 0 {
        close(fd as usize);
    }
    let pipe_ends = pipe();
    let on_pipe = pipe_ends.map(|(reader, _)| fstatvfs(reader).err());
    if let Ok((reader, writer)) = pipe_ends {
        close(reader);
        close(writer);
    }
    unlinkat(CWD, file.as_str(), 0);
    if directory >= 0 {
        close(directory as usize);
    }
    let good = before.ok().zip(on_file.ok()).zip(dev.ok()).is_some_and(|((before, after), dev)| {
        before.bsize >= 512
            && before.frsize == before.bsize
            && before.blocks > 0
            && before.bfree <= before.blocks
            && before.namemax == 255
            && before.fsid == dev
            && after.fsid == dev
            && before.flags & abi::st::NOSUID != 0
            && after.bfree < before.bfree
    });
    report.check(
        "fstatvfs-counts",
        good && on_pipe == Ok(Some(-errno::EINVAL)),
        format_args!(
            "{:?} free of {:?} blocks of {:?}, then {:?} with 256 KiB more; a pipe {on_pipe:?}",
            before.map(|stat| stat.bfree),
            before.map(|stat| stat.blocks),
            before.map(|stat| stat.bsize),
            on_file.map(|stat| stat.bfree)
        ),
    );
    let synced = feos_user::sync();
    report.check("sync-ok", synced == 0, format_args!("sync {synced}"));
}


/// The time `--persist-write` sets and `--persist-check` looks for: past the second, to
/// the nanosecond.
const PERSIST_TIME: abi::Timespec = abi::Timespec { sec: 1_700_000_000, nsec: 123_456_789 };

/// `--persist-write DIR`: a file renamed over another with `RENAMEAT`, its times set to
/// the nanosecond, and `SYNC`, for the boot test to turn the machine off after.
fn persist_write(dir: &str) -> i32 {
    let (old, new) = (text!("{dir}/persist-old"), text!("{dir}/persist-new"));
    let made = make_file(old.as_str(), b"the old one");
    let made_new = make_file(new.as_str(), b"the new one");
    let renamed = renameat(CWD, new.as_str(), CWD, old.as_str(), 0);
    let timed = utimensat(CWD, old.as_str(), Some(&[PERSIST_TIME, PERSIST_TIME]), 0);
    let synced = feos_user::sync();
    let good = made == 0 && made_new == 0 && renamed == 0 && timed == 0 && synced == 0;
    println!("ftest persist-write: {} (made {made} {made_new}, renamed {renamed}, times {timed}, sync {synced})", if good { "ok" } else { "FAILED" });
    i32::from(!good)
}

/// `--persist-check DIR`: what `--persist-write` left, after a reboot.
fn persist_check(dir: &str) -> i32 {
    let (old, new) = (text!("{dir}/persist-old"), text!("{dir}/persist-new"));
    let mut out = [0u8; 32];
    let got = slurp(old.as_str(), &mut out);
    let times = fstatat(CWD, old.as_str(), 0).map(|stat| (stat.atime, stat.mtime));
    let gone = fstatat(CWD, new.as_str(), 0).err();
    let good = &out[..got.max(0) as usize] == b"the new one" && times == Ok((PERSIST_TIME, PERSIST_TIME)) && gone == Some(-errno::ENOENT);
    println!("ftest persist-check: {} (read {got} bytes, times {times:?}, the old name {gone:?})", if good { "ok" } else { "FAILED" });
    unlinkat(CWD, old.as_str(), 0);
    i32::from(!good)
}

/// Every name the boot test made for these, and the checks' own, gone again.
/// The `count` values after `flag` among the arguments.
fn values<const COUNT: usize>(args: &Args, flag: &str) -> Option<[&'static str; COUNT]> {
    let at = (0..args.len()).find(|&index| args.get(index) == Some(flag))?;
    let mut out = [""; COUNT];
    for (index, value) in out.iter_mut().enumerate() {
        *value = args.get(at + 1 + index)?;
    }
    Some(out)
}

/// What a lock child exits with: 0 for done, 11 for `EAGAIN`, 2 for anything else.
fn lock_status(result: isize) -> i32 {
    match result {
        0 => 0,
        error if error == -errno::EAGAIN => 11,
        error => {
            println!("ftest lock child: {error}");
            2
        }
    }
}

const fn lock_kind(name: &str) -> i16 {
    match name.as_bytes() {
        [b'r', ..] => f::RDLCK,
        [b'w', ..] => f::WRLCK,
        _ => f::UNLCK,
    }
}

/// `--setlk PATH KIND START LENGTH`: one `F_SETLK` as another process, of `r`, `w` or
/// `u`, the file opened for both.
fn setlk_child(args: &Args) -> i32 {
    let Some([path, kind, start, length]) = values::<4>(args, "--setlk") else {
        return 2;
    };
    let (Some(start), Some(length)) = (parse_number(start), parse_number(length)) else {
        return 2;
    };
    let fd = openat(CWD, path, o::RDWR, 0);
    if fd < 0 {
        return 3;
    }
    let mut lock = record(lock_kind(kind), start as i64, length as i64);
    lock_status(fcntl_lock(fd as usize, f::SETLK, &mut lock))
}

/// `--getlk PATH START LENGTH KIND PID`: `F_GETLK` of a write lock over the range as
/// another process, which must find a lock of `KIND` held by `PID` (`u` for none).
fn getlk_child(args: &Args) -> i32 {
    let Some([path, start, length, kind, pid]) = values::<5>(args, "--getlk") else {
        return 2;
    };
    let (Some(start), Some(length), Some(pid)) = (parse_number(start), parse_number(length), parse_number(pid)) else {
        return 2;
    };
    let fd = openat(CWD, path, o::RDONLY, 0);
    if fd < 0 {
        return 3;
    }
    let mut lock = record(f::WRLCK, start as i64, length as i64);
    let result = fcntl_lock(fd as usize, f::GETLK, &mut lock);
    let expected = lock_kind(kind);
    let good = result == 0 && lock.kind == expected && (expected == f::UNLCK || lock.pid as usize == pid);
    println!("ftest getlk child: {result} kind={} whence={} start={} length={} pid={}", lock.kind, lock.whence, lock.start, lock.length, lock.pid);
    i32::from(!good)
}

/// `--flock-wait PATH`: as `flock-exclusive-blocks`' second process: `LOCK_EX | LOCK_NB`
/// must be refused, then says `w` on its standard output, waits for the lock, and says
/// `g` once it has it.
fn flock_wait_child(path: &str) -> i32 {
    let fd = openat(CWD, path, o::RDONLY, 0);
    if fd < 0 {
        return 3;
    }
    let fd = fd as usize;
    if flock(fd, lock::EX | lock::NB) != -errno::EAGAIN {
        write(1, b"x");
        return 4;
    }
    write(1, b"w");
    let got = flock(fd, lock::EX);
    write(1, if got == 0 { b"g" } else { b"x" });
    lock_status(got)
}

/// `--flock-exit PATH`: takes an exclusive `flock` and ends without letting go.
fn flock_exit_child(path: &str) -> i32 {
    let fd = openat(CWD, path, o::RDONLY, 0);
    if fd < 0 {
        return 3;
    }
    lock_status(flock(fd as usize, lock::EX | lock::NB))
}

/// SQLite's lock bytes (its `os.h`): one each for pending and reserved, and 510 that
/// readers share, a gigabyte in.
const PENDING_BYTE: i64 = 0x4000_0000;
const RESERVED_BYTE: i64 = PENDING_BYTE + 1;
const SHARED_FIRST: i64 = PENDING_BYTE + 2;
const SHARED_SIZE: i64 = 510;

/// SQLite's way to a `SHARED` lock: a read lock on the pending byte, so a writer that has
/// it keeps new readers out, a read lock on the shared bytes, and the pending byte let go.
fn sqlite_shared(fd: usize) -> isize {
    let got = fcntl_lock(fd, f::SETLK, &mut record(f::RDLCK, PENDING_BYTE, 1));
    if got != 0 {
        return got;
    }
    let shared = fcntl_lock(fd, f::SETLK, &mut record(f::RDLCK, SHARED_FIRST, SHARED_SIZE));
    fcntl_lock(fd, f::SETLK, &mut record(f::UNLCK, PENDING_BYTE, 1));
    shared
}

/// `--sqlite-shared PATH`: whether another connection can get `SHARED` now.
fn sqlite_shared_child(path: &str) -> i32 {
    let fd = openat(CWD, path, o::RDWR, 0);
    if fd < 0 {
        return 3;
    }
    lock_status(sqlite_shared(fd as usize))
}

/// `--sqlite-hold PATH`: another connection reading: `SHARED`, says `s`, and keeps it
/// until its standard input ends.
fn sqlite_hold_child(path: &str) -> i32 {
    let fd = openat(CWD, path, o::RDWR, 0);
    if fd < 0 {
        return 3;
    }
    let got = sqlite_shared(fd as usize);
    write(1, if got == 0 { b"s" } else { b"x" });
    let mut byte = [0u8; 1];
    while read(0, &mut byte) > 0 {}
    lock_status(got)
}

/// Runs `program` with `args` and gives its exit status.
fn run(program: &str, args: &[&str]) -> isize {
    let child = spawn(program, args);
    if child > 0 {
        wait(child as usize)
    } else {
        child
    }
}

/// Another process's `F_SETLK` on `path`: 0, or 11 for `EAGAIN`.
fn other_setlk(program: &str, path: &str, kind: &str, start: i64, length: i64) -> isize {
    run(program, &[program, "--setlk", path, kind, text!("{start}").as_str(), text!("{length}").as_str()])
}

/// `flock` (stage 1c.5): one description's lock, shared by its descriptors and gone with
/// the last of them, shared and exclusive between processes, waiting, and the end of a
/// process.
fn test_flock(report: &mut Report, program: &str, dir: &str) {
    let path = text!("{dir}/ftest-flock");
    let made = openat(CWD, path.as_str(), o::RDWR | o::CREAT | o::TRUNC, 0o644);
    if made < 0 {
        report.check("flock-exclusive-blocks", false, format_args!("open: {made}"));
        return;
    }
    let first = made as usize;
    let second = openat(CWD, path.as_str(), o::RDONLY, 0) as usize;

    // Another description in this process is in the way as another process's would be;
    // the same description, through another descriptor, isn't, and keeps the lock until
    // its last descriptor closes.
    let ex = flock(first, lock::EX);
    let refused = flock(second, lock::EX | lock::NB);
    let refused_shared = flock(second, lock::SH | lock::NB);
    let copy = dup(first) as usize;
    let again = flock(copy, lock::EX | lock::NB);
    close(first);
    let after_one = flock(second, lock::EX | lock::NB);
    close(copy);
    let after_last = flock(second, lock::EX | lock::NB);
    report.check(
        "flock-nb-eagain",
        ex == 0 && refused == -errno::EAGAIN && refused_shared == -errno::EAGAIN && again == 0 && after_one == -errno::EAGAIN && after_last == 0,
        format_args!("ex={ex} other={refused},{refused_shared} dup={again} one-closed={after_one} all-closed={after_last}"),
    );
    flock(second, lock::UN);

    // Shared with shared, not with exclusive; and directories lock too.
    let third = openat(CWD, path.as_str(), o::WRONLY, 0) as usize;
    let a = flock(second, lock::SH);
    let b = flock(third, lock::SH | lock::NB);
    let fourth = openat(CWD, path.as_str(), o::RDONLY, 0) as usize;
    let c = flock(fourth, lock::EX | lock::NB);
    flock(second, lock::UN);
    let d = flock(fourth, lock::EX | lock::NB);
    flock(third, lock::UN | lock::NB);
    let e = flock(fourth, lock::EX | lock::NB);
    let dir_a = openat(CWD, dir, o::RDONLY | o::DIRECTORY, 0);
    let dir_b = openat(CWD, dir, o::RDONLY | o::DIRECTORY, 0);
    let (dir_ex, dir_refused) = if dir_a >= 0 && dir_b >= 0 {
        (flock(dir_a as usize, lock::EX), flock(dir_b as usize, lock::SH | lock::NB))
    } else {
        (dir_a.min(dir_b), 0)
    };
    for fd in [dir_a, dir_b] {
        if fd >= 0 {
            close(fd as usize);
        }
    }
    report.check(
        "flock-shared-shares",
        a == 0 && b == 0 && c == -errno::EAGAIN && d == -errno::EAGAIN && e == 0 && dir_ex == 0 && dir_refused == -errno::EAGAIN,
        format_args!("sh={a},{b} ex-over-sh={c} ex-over-one={d} ex-alone={e} dir={dir_ex},{dir_refused}"),
    );
    close(third);
    close(fourth);

    // Two processes, with a pipe to say how far the second has got: it waits for the
    // lock, and has it only once the first lets go.
    let ex = flock(second, lock::EX);
    let (mut said, mut waited, mut status, mut early) = ([0u8; 2], false, -1, 0);
    if let Ok((reader, writer)) = pipe() {
        let child = spawn_io(program, &[program, "--flock-wait", path.as_str()], abi::INHERIT, writer);
        close(writer);
        if child > 0 {
            let first = read(reader, &mut said[..1]);
            // Long enough for the child to be in its wait; nothing more may come meanwhile.
            sleep_ms(200);
            fcntl(reader, f::SETFL, o::NONBLOCK);
            early = read(reader, &mut said[1..]);
            fcntl(reader, f::SETFL, 0);
            flock(second, lock::UN);
            waited = first == 1 && early == -errno::EAGAIN && read(reader, &mut said[1..]) == 1;
            status = wait(child as usize);
        }
        close(reader);
    }
    report.check(
        "flock-exclusive-blocks",
        ex == 0 && waited && &said == b"wg" && status == 0,
        format_args!("ex={ex} said={:?} meanwhile={early} child={status}", core::str::from_utf8(&said)),
    );

    // A process that ends holding one lets go of it.
    let status = run(program, &[program, "--flock-exit", path.as_str()]);
    let after = flock(second, lock::EX | lock::NB);
    report.check("flock-released-at-exit", status == 0 && after == 0, format_args!("child={status} then={after}"));
    close(second);
    unlink(path.as_str());
}

/// Record locks (stage 1c.5): conflicts between processes, `F_GETLK` naming the holder,
/// every lock on a file going with any of its descriptors, the padding of `struct flock`,
/// and the byte locks SQLite takes.
fn test_record_locks(report: &mut Report, program: &str, dir: &str) {
    let path = text!("{dir}/ftest-records");
    let made = openat(CWD, path.as_str(), o::RDWR | o::CREAT | o::TRUNC, 0o644);
    if made < 0 {
        report.check("fcntl-setlk-conflict", false, format_args!("open: {made}"));
        return;
    }
    let fd = made as usize;
    let path = path.as_str();
    let pid = feos_user::getpid();

    let write_lock = fcntl_lock(fd, f::SETLK, &mut record(f::WRLCK, 0, 10));
    let read_lock = fcntl_lock(fd, f::SETLK, &mut record(f::RDLCK, 20, 10));
    let over_write = other_setlk(program, path, "r", 5, 1);
    let shares_read = other_setlk(program, path, "r", 20, 5);
    let over_read = other_setlk(program, path, "w", 25, 1);
    let between = other_setlk(program, path, "w", 10, 10);
    let to_the_end = other_setlk(program, path, "w", 30, 0);
    report.check(
        "fcntl-setlk-conflict",
        write_lock == 0 && read_lock == 0 && over_write == 11 && shares_read == 0 && over_read == 11 && between == 0 && to_the_end == 0,
        format_args!("set={write_lock},{read_lock} r-over-w={over_write} r-over-r={shares_read} w-over-r={over_read} between={between} after={to_the_end}"),
    );

    // Another process is told who's in the way, and where; this one, of nothing, since
    // its own locks never are.
    let pid_text = text!("{pid}");
    let theirs = run(program, &[program, "--getlk", path, "3", "2", "w", pid_text.as_str()]);
    let free = run(program, &[program, "--getlk", path, "12", "5", "u", "0"]);
    let mut own = record(f::WRLCK, 0, 0);
    let mine = fcntl_lock(fd, f::GETLK, &mut own);
    // `F_UNLCK` is nothing to ask about.
    let unlock_asked = fcntl_lock(fd, f::GETLK, &mut record(f::UNLCK, 0, 0));
    report.check(
        "fcntl-getlk-reports-pid",
        theirs == 0 && free == 0 && mine == 0 && own.kind == f::UNLCK && unlock_asked == -errno::EINVAL,
        format_args!("other-sees={theirs} free={free} own={mine}:{} getlk-unlck={unlock_asked}", own.kind),
    );

    // Any descriptor for the file closing takes every lock this process has on it.
    let other = openat(CWD, path, o::RDONLY, 0);
    let before = other_setlk(program, path, "w", 0, 1);
    if other >= 0 {
        close(other as usize);
    }
    let after = other_setlk(program, path, "w", 0, 0);
    report.check(
        "fcntl-locks-dropped-on-close",
        other >= 0 && before == 11 && after == 0,
        format_args!("held={before} after-another-fd-closed={after}"),
    );

    // Garbage in both padding fields: ignored, and zero in what comes back.
    let mut raw = record(f::WRLCK, 100, 1).to_bytes();
    raw[4..8].copy_from_slice(&0xdead_beef_u32.to_ne_bytes());
    raw[28..32].copy_from_slice(&0xcafe_f00d_u32.to_ne_bytes());
    let set = fcntl(fd, f::SETLK, raw.as_mut_ptr() as usize);
    let held = other_setlk(program, path, "w", 100, 1);
    let mut asked = record(f::WRLCK, 0, 0).to_bytes();
    asked[4..8].fill(0xff);
    asked[28..32].fill(0xff);
    let looked = fcntl(fd, f::GETLK, asked.as_mut_ptr() as usize);
    let zeroed = asked[4..8] == [0; 4] && asked[28..32] == [0; 4];
    report.check(
        "flock-struct-padding-ignored",
        set == 0 && held == 11 && looked == 0 && zeroed,
        format_args!("set={set} other={held} getlk={looked} padding-zeroed={zeroed}"),
    );
    fcntl_lock(fd, f::SETLK, &mut record(f::UNLCK, 0, 0));

    // SQLite's sequence, one connection here and others as other processes: SHARED,
    // RESERVED (readers still welcome, no second writer), PENDING (no new readers),
    // EXCLUSIVE once the last reader has gone, and back to nothing.
    let shared = sqlite_shared(fd);
    let other_shared = run(program, &[program, "--sqlite-shared", path]);
    let reserved = fcntl_lock(fd, f::SETLK, &mut record(f::WRLCK, RESERVED_BYTE, 1));
    let reader_while_reserved = run(program, &[program, "--sqlite-shared", path]);
    let second_writer = other_setlk(program, path, "w", RESERVED_BYTE, 1);
    let (mut holding, mut exclusive_refused, mut held_status) = ([0u8; 1], 0, -1);
    let (mut pending, mut new_reader, mut exclusive) = (-1, -1, -1);
    if let (Ok((in_read, in_write)), Ok((out_read, out_write))) = (pipe(), pipe()) {
        let child = spawn_io(program, &[program, "--sqlite-hold", path], in_read, out_write);
        close(in_read);
        close(out_write);
        if child > 0 && read(out_read, &mut holding) == 1 {
            pending = fcntl_lock(fd, f::SETLK, &mut record(f::WRLCK, PENDING_BYTE, 1));
            new_reader = run(program, &[program, "--sqlite-shared", path]);
            exclusive_refused = fcntl_lock(fd, f::SETLK, &mut record(f::WRLCK, SHARED_FIRST, SHARED_SIZE));
        }
        close(in_write);
        if child > 0 {
            held_status = wait(child as usize);
        }
        close(out_read);
        exclusive = fcntl_lock(fd, f::SETLK, &mut record(f::WRLCK, SHARED_FIRST, SHARED_SIZE));
    }
    let reader_while_exclusive = run(program, &[program, "--sqlite-shared", path]);
    let released = fcntl_lock(fd, f::SETLK, &mut record(f::UNLCK, 0, 0));
    let reader_after = run(program, &[program, "--sqlite-shared", path]);
    let writer_after = other_setlk(program, path, "w", 0, 0);
    report.check(
        "sqlite-style",
        shared == 0
            && other_shared == 0
            && reserved == 0
            && reader_while_reserved == 0
            && second_writer == 11
            && &holding == b"s"
            && pending == 0
            && new_reader == 11
            && exclusive_refused == -errno::EAGAIN
            && held_status == 0
            && exclusive == 0
            && reader_while_exclusive == 11
            && released == 0
            && reader_after == 0
            && writer_after == 0,
        format_args!(
            "shared={shared},{other_shared} reserved={reserved} reader={reader_while_reserved} writer={second_writer} \
             pending={pending} new-reader={new_reader} exclusive={exclusive_refused}->{exclusive} \
             reader-then={reader_while_exclusive} released={released} after={reader_after},{writer_after}"
        ),
    );
    close(fd);
    unlink(path);
}

fn tidy(dir: &str) {
    for name in ["ftest-at/inside", "ftest-at/excl", "ftest-real/file"] {
        unlinkat(CWD, text!("{dir}/{name}").as_str(), 0);
    }
    for name in ["ftest-at/sub", "ftest-at", "ftest-real/sub", "ftest-real"] {
        unlinkat(CWD, text!("{dir}/{name}").as_str(), at::REMOVEDIR);
    }
}

fn main(args: Args) -> i32 {
    if let Some(expected) = args.value_of("--drain").and_then(parse_number) {
        return drain(expected);
    }
    if args.has("--dev-fd") {
        return dev_fd_child();
    }
    if args.has("--console-user") {
        return console_user_child();
    }
    if let Some(path) = args.value_of("--sticky") {
        return sticky_child(path);
    }
    if let Some(path) = args.value_of("--chown-user") {
        return chown_child(path);
    }
    if let Some(dir) = args.value_of("--persist-write") {
        return persist_write(dir);
    }
    if let Some(dir) = args.value_of("--persist-check") {
        return persist_check(dir);
    }
    if let Some(path) = args.value_of("--append") {
        let mark = args.value_of("--mark").and_then(|mark| mark.bytes().next()).unwrap_or(b'?');
        return append(path, mark);
    }
    if args.has("--setlk") {
        return setlk_child(&args);
    }
    if args.has("--getlk") {
        return getlk_child(&args);
    }
    if let Some(path) = args.value_of("--flock-wait") {
        return flock_wait_child(path);
    }
    if let Some(path) = args.value_of("--flock-exit") {
        return flock_exit_child(path);
    }
    if let Some(path) = args.value_of("--sqlite-shared") {
        return sqlite_shared_child(path);
    }
    if let Some(path) = args.value_of("--sqlite-hold") {
        return sqlite_hold_child(path);
    }
    let dir = args.value_of("--dir").unwrap_or("/tmp");
    let program = "/bin/ftest";
    let mut report = Report { failed: 0 };
    test_dup(&mut report, dir);
    test_pipes(&mut report, program);
    test_big_files(&mut report, dir);
    test_directory(&mut report, dir);
    test_close_range(&mut report);
    test_many(&mut report, dir);
    test_append(&mut report, program, dir);
    test_openat(&mut report, dir);
    test_getdents(&mut report, dir);
    test_tree(&mut report, dir);
    test_cwd(&mut report, dir);
    test_exe(&mut report);
    test_dev(&mut report);
    test_dev_links(&mut report, program);
    test_proc(&mut report, dir);
    test_sticky(&mut report, program);
    test_etc(&mut report);
    let keeps = Keeps { coarse: args.has("--coarse"), hard_links: args.has("--hard-links") };
    test_rename(&mut report, dir);
    test_links(&mut report, dir, keeps);
    test_attributes(&mut report, program, dir, keeps);
    test_statvfs(&mut report, dir);
    test_flock(&mut report, program, dir);
    test_record_locks(&mut report, program, dir);
    tidy(dir);
    if report.failed == 0 {
        println!("ftest: all ok");
        0
    } else {
        println!("ftest: {} FAILED", report.failed);
        1
    }
}

entry!(main);
