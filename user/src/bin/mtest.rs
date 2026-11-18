//! Checks the memory calls: `mmap2`, `munmap`, `mprotect`, `madvise`, `msync`, and v1's
//! `mmap` beside them. Ends with `mtest: all ok`, or says which failed.
//!
//! Some checks are about frames: a reservation that costs none, a populated mapping that
//! costs them all at once. A program can't see the machine's free frames until
//! `SYSINFO`, so those parts run only when `ktest drive /bin/mtest` runs this and answers
//! [`free_frames`]; otherwise they say they're skipped.
//!
//! File mappings are checked on files in `--dir DIR` (`/tmp` by default), so a boot test
//! can run them on each kind of mount. `--ino PATH` prints a file's inode number, for a
//! boot test to compare across a rename (there's no call to rename until `RENAMEAT`).
//! `--cache-pressure MIB` maps files totalling that much in `DIR`, more than the machine
//! has free, reads them through, and checks a promised page still gets its frame.
//! `--persist PATH TEXT` writes `TEXT` into `PATH` through a shared mapping and `MSYNC`s
//! it, so a boot test can read it with a kernel command, which sees only what was written
//! back, and again after pulling the plug.

#![no_std]
#![no_main]

use feos_user::abi::{
    errno, madv, map, ms, open as flags, prot, seek, MAX_VMAS, PAGE_SIZE, USER_ARGS_SIZE, USER_MEMORY_LIMIT, USER_MMAP_BASE,
    USER_STACK_SIZE, USER_STACK_TOP,
};
use feos_user::{
    close, entry, free_frames, madvise, mmap, mmap2, mmap2_file, mprotect, msync, munmap, open, println, spawn, spawn_io,
    stack_pointer, wait, write as write_fd, Args,
};

const GIB: usize = 1 << 30;
const RW: usize = prot::READ | prot::WRITE;
const PRIVATE: usize = map::PRIVATE;

/// Where `mmap` hands out memory from.
const AREA: usize = USER_MMAP_BASE;
/// The bottom of the argument area, and the top of the stack.
const ARGS_LOW: usize = USER_STACK_TOP - USER_ARGS_SIZE;

struct Checks {
    failed: usize,
}

impl Checks {
    fn check(&mut self, name: &str, good: bool, detail: core::fmt::Arguments<'_>) {
        if good {
            println!("mtest {name}: ok ({detail})");
        } else {
            self.failed += 1;
            println!("mtest {name}: FAILED ({detail})");
        }
    }
}

fn read(address: usize) -> u8 {
    // SAFETY: each caller's own mapping.
    unsafe { core::ptr::read_volatile(address as *const u8) }
}

fn write(address: usize, value: u8) {
    // SAFETY: each caller's own mapping.
    unsafe { core::ptr::write_volatile(address as *mut u8, value) }
}

fn all(address: usize, length: usize, value: u8) -> bool {
    (0..length).step_by(97).chain([length - 1]).all(|offset| read(address + offset) == value)
}

/// Runs `mtest --child NAME --dir DIR` and returns its status.
fn child_in(name: &str, dir: &str) -> isize {
    let pid = spawn("/bin/mtest", &["/bin/mtest", "--child", name, "--dir", dir]);
    if pid <= 0 {
        return pid;
    }
    wait(pid as usize)
}

/// Runs `mtest --child NAME` and returns its status.
fn child(name: &str) -> isize {
    let pid = spawn("/bin/mtest", &["/bin/mtest", "--child", name]);
    if pid <= 0 {
        return pid;
    }
    wait(pid as usize)
}

/// A path in a directory, in a buffer of the caller's.
fn join<'a>(dir: &str, name: &str, out: &'a mut [u8; 256]) -> &'a str {
    let total = (dir.len() + 1 + name.len()).min(out.len());
    let mut at = 0;
    for &byte in dir.as_bytes().iter().chain(b"/").chain(name.as_bytes()).take(total) {
        out[at] = byte;
        at += 1;
    }
    core::str::from_utf8(&out[..at]).unwrap_or("/")
}

/// Makes `path` hold `data`, from its start, replacing what was there.
fn put_file(path: &str, data: &[u8]) -> bool {
    let fd = open(path, flags::WRITE | flags::CREATE | flags::TRUNCATE);
    if fd < 0 {
        return false;
    }
    let mut done = 0;
    while done < data.len() {
        let end = (done + 4096).min(data.len());
        if write_fd(fd as usize, &data[done..end]) != (end - done) as isize {
            close(fd as usize);
            return false;
        }
        done = end;
    }
    close(fd as usize) == 0
}

/// A file `pages` pages long (plus `extra` bytes) whose byte at each offset is `pattern`.
fn make_file(path: &str, pages: usize, extra: usize, pattern: fn(usize) -> u8) -> bool {
    let fd = open(path, flags::WRITE | flags::CREATE | flags::TRUNCATE);
    if fd < 0 {
        return false;
    }
    let mut chunk = [0u8; 4096];
    let total = pages * PAGE_SIZE + extra;
    let mut done = 0;
    let mut good = true;
    while done < total {
        let count = (total - done).min(chunk.len());
        for (offset, byte) in chunk[..count].iter_mut().enumerate() {
            *byte = pattern(done + offset);
        }
        good &= write_fd(fd as usize, &chunk[..count]) == count as isize;
        done += count;
    }
    close(fd as usize);
    good
}

fn pattern(offset: usize) -> u8 {
    (offset / 3) as u8 ^ (offset >> 12) as u8
}

/// Machine code that returns `value`.
#[cfg(target_arch = "aarch64")]
fn returning(value: u16) -> [u8; 8] {
    let mov = 0x5280_0000u32 | (u32::from(value) << 5);
    let ret = 0xd65f_03c0u32;
    let mut code = [0u8; 8];
    code[..4].copy_from_slice(&mov.to_le_bytes());
    code[4..].copy_from_slice(&ret.to_le_bytes());
    code
}

/// Machine code that returns `value`.
#[cfg(target_arch = "x86_64")]
fn returning(value: u16) -> [u8; 8] {
    let value = u32::from(value).to_le_bytes();
    [0xb8, value[0], value[1], value[2], value[3], 0xc3, 0xcc, 0xcc]
}

/// Calls the code at `address`.
fn call(address: usize) -> u32 {
    // SAFETY: each caller's own executable mapping of code from `returning`.
    let code: extern "C" fn() -> u32 = unsafe { core::mem::transmute(address) };
    code()
}

/// Goes down the stack a kilobyte a call, every byte of each frame written, until the
/// stack pointer is at or below `until`, and returns the lowest it saw.
#[inline(never)]
fn descend(until: usize) -> usize {
    let mut frame = [0u8; 1024];
    frame[1023] = 1;
    core::hint::black_box(&mut frame);
    let here = stack_pointer();
    let lowest = if here > until { descend(until) } else { here };
    // Not a tail call: the frame is used after.
    lowest - usize::from(core::hint::black_box(frame[1023]) - 1)
}

/// How near the bottom of the stack `stack-overflow` says where it is.
const OVERFLOW_MARK: usize = 64 * 1024;

/// Goes down the stack a kilobyte a call, as [`descend`] does but without end, and once
/// the stack pointer is at or below `mark` writes it into `path`, once.
#[inline(never)]
fn overflow(mark: usize, path: &str, told: bool) -> usize {
    let mut frame = [0u8; 1024];
    frame[1023] = 1;
    core::hint::black_box(&mut frame);
    let here = stack_pointer();
    let told = told || (here <= mark && put_file(path, &here.to_le_bytes()));
    // Never zero, but the compiler can't see that, so this isn't endless recursion to it.
    let lowest = if here > 0 { overflow(mark, path, told) } else { here };
    lowest - usize::from(core::hint::black_box(frame[1023]) - 1)
}

/// What the children do: each touches what it shouldn't be able to, and so ends with
/// status 139, or 135 for a file page that isn't there. The stack ones say 0 if all was
/// as the layout says.
fn run_child(name: &str, dir: &str, args: &Args) -> i32 {
    let mut buffer = [0u8; 256];
    match name {
        "stack-8m" => {
            // 7.5 MiB of the stack itself, all of it there. `main` starts in the argument
            // area, so that's measured from the area's bottom, not from here.
            let wanted = USER_STACK_SIZE - USER_STACK_SIZE / 16;
            let lowest = descend(ARGS_LOW - wanted);
            let used = ARGS_LOW.saturating_sub(lowest);
            println!("mtest child {name}: went {} KiB down the stack", used / 1024);
            return if used >= wanted { 0 } else { 1 };
        }
        "stack-below-args" => {
            // The first stack pointer, and this one, are in the argument area, and the
            // whole stack below it is there to use, untouched by the arguments.
            let started = args.address();
            let sp = stack_pointer();
            let inside = |at: usize| (ARGS_LOW..USER_STACK_TOP).contains(&at);
            let low = ARGS_LOW - USER_STACK_SIZE;
            let mut usable = true;
            for (n, at) in (low..ARGS_LOW).step_by(1 << 20).chain([ARGS_LOW - 1]).enumerate() {
                write(at, n as u8 + 1);
                usable &= read(at) == n as u8 + 1;
            }
            println!("mtest child {name}: started at {started:#x}, sp {sp:#x}, stack {low:#x}..{ARGS_LOW:#x} usable={usable}");
            return if inside(started) && inside(sp) && started.is_multiple_of(16) && usable { 0 } else { 1 };
        }
        "stack-overflow" => {
            // Past the 8 MiB, and into the guard. Any stack would end in 139, so on the way
            // the child writes down where it was once in the stack's last `OVERFLOW_MARK`
            // bytes, for the parent to see it got that far first.
            let path = join(dir, "mt-overflow", &mut buffer);
            core::hint::black_box(overflow(ARGS_LOW - USER_STACK_SIZE + OVERFLOW_MARK, path, false));
        }
        "file-past-end" => {
            let path = join(dir, "mt-short", &mut buffer);
            let fd = open(path, flags::READ);
            let at = mmap2_file(0, 2 * PAGE_SIZE, prot::READ, map::PRIVATE, fd as usize, 0) as usize;
            core::hint::black_box(read(at));
            core::hint::black_box(read(at + PAGE_SIZE));
        }
        "file-truncate" => {
            let path = join(dir, "mt-truncated", &mut buffer);
            let fd = open(path, flags::READ);
            let at = mmap2_file(0, 2 * PAGE_SIZE, prot::READ, map::SHARED, fd as usize, 0) as usize;
            core::hint::black_box(read(at + PAGE_SIZE));
            // Cut to nothing, through v1's `OPEN`: the page that was read goes too.
            let cut = open(path, flags::WRITE | flags::TRUNCATE);
            if cut < 0 {
                println!("mtest child {name}: truncating said {cut}");
                return 2;
            }
            core::hint::black_box(read(at + PAGE_SIZE));
        }
        "file-truncate-private" => {
            // The written page is a private copy, not the cache's, and still goes.
            let path = join(dir, "mt-truncated", &mut buffer);
            let fd = open(path, flags::READ);
            let at = mmap2_file(0, 2 * PAGE_SIZE, RW, map::PRIVATE, fd as usize, 0) as usize;
            write(at + PAGE_SIZE, 1);
            let cut = open(path, flags::WRITE | flags::TRUNCATE);
            if cut < 0 {
                println!("mtest child {name}: truncating said {cut}");
                return 2;
            }
            core::hint::black_box(read(at + PAGE_SIZE));
        }
        "write-read-only-middle" => {
            let at = mmap2(0, 3 * PAGE_SIZE, RW, PRIVATE) as usize;
            mprotect(at + PAGE_SIZE, PAGE_SIZE, prot::READ);
            write(at, 1);
            write(at + 2 * PAGE_SIZE, 1);
            write(at + PAGE_SIZE, 1);
        }
        "read-unmapped-middle" => {
            let at = mmap2(0, 3 * PAGE_SIZE, RW, PRIVATE) as usize;
            munmap(at + PAGE_SIZE, PAGE_SIZE);
            core::hint::black_box(read(at));
            core::hint::black_box(read(at + PAGE_SIZE));
        }
        "shared-read-synced" | "shared-read-unmapped" => {
            // What the parent wrote through its mappings, read with `READ`.
            let path = join(dir, "mt-shared", &mut buffer);
            let fd = open(path, flags::READ);
            let mut page = [0u8; 2 * PAGE_SIZE];
            let got = if fd >= 0 { feos_user::read(fd as usize, &mut page) } else { fd };
            let mut wanted = [(10, 0xa1), (PAGE_SIZE + 20, 0xb2), (30, pattern(30))];
            if name == "shared-read-unmapped" {
                wanted[2].1 = 0xc3;
            }
            let good = got == page.len() as isize && wanted.iter().all(|&(at, byte)| page[at] == byte);
            println!("mtest child {name}: read={got} good={good}");
            return if good { 0 } else { 1 };
        }
        "read-prot-none" => {
            let at = mmap2(0, PAGE_SIZE, prot::NONE, PRIVATE) as usize;
            core::hint::black_box(read(at));
        }
        _ => return 2,
    }
    println!("mtest child {name}: got through");
    77
}

/// The v2 layout: memory far above v1's window (64..80 GiB), a reservation of all a
/// process may charge, and the 8 MiB stack below a separate argument area. Files go in
/// `dir`.
fn layout_checks(checks: &mut Checks, dir: &str) {
    // A gigabyte at 300 GiB, where the hint says, usable once it's made so.
    let hint = 300 * GIB;
    let at = mmap2(hint, GIB, prot::NONE, PRIVATE);
    let opened = if at > 0 { mprotect(at as usize + GIB - PAGE_SIZE, PAGE_SIZE, RW) } else { at };
    if opened == 0 {
        write(at as usize + GIB - 1, 0x3c);
    }
    let good = at == hint as isize && opened == 0 && read(at as usize + GIB - 1) == 0x3c;
    if at > 0 {
        munmap(at as usize, GIB);
    }
    checks.check("mmap-above-64g", good, format_args!("hint={hint:#x} at={at:#x} mprotect={opened}"));

    // 64 GiB with no access, which costs nothing; made writable without a charge, and a
    // page touched every gigabyte, it costs those pages and their tables.
    let span = USER_MEMORY_LIMIT;
    let before = free_frames();
    let reserved = mmap2(0, span, prot::NONE, PRIVATE | map::NORESERVE);
    let opened = if reserved > 0 { mprotect(reserved as usize, span, RW) } else { reserved };
    let pages = span / GIB;
    let spot = |page: usize| reserved as usize + page * GIB + page * 8;
    if opened == 0 {
        for page in 0..pages {
            write(spot(page), page as u8 + 1);
        }
    }
    let intact = opened == 0 && (0..pages).all(|page| read(spot(page)) == page as u8 + 1);
    let after = free_frames();
    if reserved > 0 {
        munmap(reserved as usize, span);
    }
    let cost = before.zip(after).map(|(before, after)| before.saturating_sub(after));
    let cheap = cost.is_none_or(|cost| (pages..=3 * pages + 8).contains(&cost));
    checks.check(
        "mmap2-noreserve-64g",
        intact && cheap,
        format_args!("reserved={reserved:#x} mprotect={opened} cost={cost:?}"),
    );

    // The stack: 7.5 MiB of it used, all of it below the argument area, and past it the
    // guard.
    let status = child("stack-8m");
    checks.check("stack-8m", status == 0, format_args!("child={status}"));
    let status = child("stack-below-args");
    checks.check("stack-below-args", status == 0, format_args!("child={status}"));
    let mut buffer = [0u8; 256];
    let path = join(dir, "mt-overflow", &mut buffer);
    feos_user::unlink(path);
    let status = child_in("stack-overflow", dir);
    let mut said = [0u8; 8];
    let fd = open(path, flags::READ);
    let got = if fd >= 0 { feos_user::read(fd as usize, &mut said) } else { fd };
    if fd >= 0 {
        close(fd as usize);
    }
    feos_user::unlink(path);
    let reached = usize::from_le_bytes(said);
    let low = ARGS_LOW - USER_STACK_SIZE;
    let deep = got == 8 && (low..=low + OVERFLOW_MARK).contains(&reached);
    checks.check(
        "stack-overflow-faults",
        status == 139 && deep,
        format_args!("child={status} reached={reached:#x} (stack bottom {low:#x})"),
    );
}

/// File mappings: private copy on write, shared read-only, past the end, truncation,
/// running a file's code, and `ETXTBSY` both ways. In `dir`.
fn file_checks(checks: &mut Checks, dir: &str) {
    let mut buffer = [0u8; 256];
    let path = join(dir, "mt-file", &mut buffer);
    let made = make_file(path, 3, 100, pattern);
    let reader = open(path, flags::READ);
    if !made || reader < 0 {
        checks.check("mmap-file-setup", false, format_args!("made={made} open={reader}"));
        return;
    }
    let reader = reader as usize;

    // Private, from the second page: what the file holds there.
    let at = mmap2_file(0, 2 * PAGE_SIZE, prot::READ, map::PRIVATE, reader, PAGE_SIZE);
    let good = at > 0 && (0..2 * PAGE_SIZE).step_by(37).all(|offset| read(at as usize + offset) == pattern(PAGE_SIZE + offset));
    checks.check("mmap-file-private-reads", good, format_args!("at={at:#x}"));
    if at > 0 {
        munmap(at as usize, 2 * PAGE_SIZE);
    }

    // Written privately: the copy changes, the file doesn't.
    let at = mmap2_file(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE, reader, 0);
    let mut first = [0u8; 8];
    let good = at > 0 && {
        let at = at as usize;
        let before = read(at + 10);
        write(at + 10, before ^ 0xff);
        feos_user::seek(reader, 0, seek::SET);
        feos_user::read(reader, &mut first);
        read(at + 10) == before ^ 0xff && first[..8].iter().enumerate().all(|(offset, &byte)| byte == pattern(offset))
            && before == pattern(10)
    };
    checks.check("mmap-file-private-cow", good, format_args!("at={at:#x} file={first:?}"));
    if at > 0 {
        munmap(at as usize, PAGE_SIZE);
    }

    // Shared: a write to the file shows in the mapping at once.
    let at = mmap2_file(0, PAGE_SIZE, prot::READ, map::SHARED, reader, 0);
    let writer = open(path, flags::WRITE);
    let good = at > 0 && writer >= 0 && {
        let at = at as usize;
        let before = read(at + 20);
        feos_user::seek(writer as usize, 20, seek::SET);
        write_fd(writer as usize, &[before ^ 0x5a]) == 1 && read(at + 20) == before ^ 0x5a
    };
    checks.check("mmap-file-shared-sees-write", good, format_args!("at={at:#x} writer={writer}"));
    if writer >= 0 {
        close(writer as usize);
    }
    if at > 0 {
        munmap(at as usize, PAGE_SIZE);
    }

    // The last page's tail reads as zeroes.
    let at = mmap2_file(0, PAGE_SIZE, prot::READ, map::PRIVATE, reader, 3 * PAGE_SIZE);
    let good = at > 0
        && read(at as usize + 99) == pattern(3 * PAGE_SIZE + 99)
        && (100..PAGE_SIZE).step_by(13).chain([PAGE_SIZE - 1]).all(|offset| read(at as usize + offset) == 0);
    checks.check("mmap-file-tail-zero", good, format_args!("at={at:#x}"));
    if at > 0 {
        munmap(at as usize, PAGE_SIZE);
    }
    close(reader);

    // A page wholly past the end, and one cut away by a truncation: SIGBUS, 135.
    let mut short = [0u8; 256];
    let made = put_file(join(dir, "mt-short", &mut short), &[7u8; 100]);
    let status = child_in("file-past-end", dir);
    checks.check("mmap-file-past-end-sigbus", made && status == 135, format_args!("child={status}"));
    let mut cut = [0u8; 256];
    let made = make_file(join(dir, "mt-truncated", &mut cut), 2, 0, pattern);
    let status = child_in("file-truncate", dir);
    checks.check("mmap-file-truncate-sigbus", made && status == 135, format_args!("child={status}"));
    let made = make_file(join(dir, "mt-truncated", &mut cut), 2, 0, pattern);
    let status = child_in("file-truncate-private", dir);
    checks.check("mmap-file-truncate-private-sigbus", made && status == 135, format_args!("child={status}"));

    // A file written from a mapping of the very page being written: the cached page is
    // the source and the place it's copied to, and has to end up as the file does.
    let mut own = [0u8; 256];
    let own = join(dir, "mt-self", &mut own);
    let made = make_file(own, 1, 0, pattern);
    let reader = open(own, flags::READ);
    let writer = open(own, flags::WRITE);
    let at = mmap2_file(0, PAGE_SIZE, prot::READ, map::SHARED, reader as usize, 0);
    let mut back = [0u8; 100];
    let good = made && reader >= 0 && writer >= 0 && at > 0 && {
        // SAFETY: the mapping just made, a page long.
        let source = unsafe { core::slice::from_raw_parts(at as usize as *const u8, 100) };
        feos_user::seek(writer as usize, 50, seek::SET);
        let wrote = write_fd(writer as usize, source);
        feos_user::seek(reader as usize, 50, seek::SET);
        let read_back = feos_user::read(reader as usize, &mut back);
        wrote == 100
            && read_back == 100
            && back.iter().enumerate().all(|(offset, &byte)| byte == pattern(offset))
            && (0..100).all(|offset| read(at as usize + 50 + offset) == pattern(offset))
    };
    checks.check("mmap-file-write-from-itself", good, format_args!("at={at:#x} back={:?}", &back[..8]));
    if at > 0 {
        munmap(at as usize, PAGE_SIZE);
    }
    for fd in [reader, writer] {
        if fd >= 0 {
            close(fd as usize);
        }
    }

    // Code from a file runs; rewritten, the new code runs, synced again.
    let mut code_path = [0u8; 256];
    let code = join(dir, "mt-code", &mut code_path);
    let made = put_file(code, &returning(41));
    let fd = open(code, flags::READ);
    let at = mmap2_file(0, PAGE_SIZE, prot::READ | prot::EXEC, map::PRIVATE, fd as usize, 0);
    let first = if at > 0 { call(at as usize) } else { 0 };
    checks.check("mmap-file-exec", made && first == 41, format_args!("at={at:#x} returned={first}"));
    if at > 0 {
        munmap(at as usize, PAGE_SIZE);
    }
    // Rewritten in place, not replaced: the cached page changes, so its instructions
    // have to be synced again before they run.
    let writer = open(code, flags::WRITE);
    let rewritten = writer >= 0 && write_fd(writer as usize, &returning(42)) == 8;
    if writer >= 0 {
        close(writer as usize);
    }
    let again = mmap2_file(0, PAGE_SIZE, prot::READ | prot::EXEC, map::PRIVATE, fd as usize, 0);
    let second = if again > 0 { call(again as usize) } else { 0 };
    checks.check("exec-sees-rewritten-file", rewritten && second == 42, format_args!("returned={second}"));

    // `ETXTBSY`: mapped to run, it can't be opened to write, nor truncated.
    let refused = open(code, flags::WRITE);
    let truncated = open(code, flags::WRITE | flags::TRUNCATE);
    if again > 0 {
        munmap(again as usize, PAGE_SIZE);
    }
    let allowed = open(code, flags::WRITE);
    checks.check(
        "etxtbsy-open-write",
        refused == -errno::ETXTBSY && truncated == -errno::ETXTBSY && allowed >= 0,
        format_args!("open={refused} truncate={truncated} after-munmap={allowed}"),
    );
    // And open to write, it can't be mapped to run.
    let busy = mmap2_file(0, PAGE_SIZE, prot::READ | prot::EXEC, map::PRIVATE, fd as usize, 0);
    if allowed >= 0 {
        close(allowed as usize);
    }
    let free = mmap2_file(0, PAGE_SIZE, prot::READ | prot::EXEC, map::PRIVATE, fd as usize, 0);
    checks.check(
        "etxtbsy-map-exec",
        busy == -errno::ETXTBSY && free > 0,
        format_args!("while-open={busy} after-close={free:#x}"),
    );
    if free > 0 {
        munmap(free as usize, PAGE_SIZE);
    }

    // A private page that's been written can't be made to run.
    let at = mmap2_file(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE, fd as usize, 0);
    let result = if at > 0 {
        write(at as usize, 0xc3);
        mprotect(at as usize, PAGE_SIZE, prot::READ | prot::EXEC)
    } else {
        at
    };
    checks.check("mprotect-file-exec-after-write", result == -errno::EACCES, format_args!("mprotect={result}"));
    if at > 0 {
        munmap(at as usize, PAGE_SIZE);
    }
    close(fd as usize);
    shared_checks(checks, dir);
    for name in ["mt-file", "mt-short", "mt-truncated", "mt-code", "mt-self", "mt-shared", "mt-shm"] {
        let mut gone = [0u8; 256];
        feos_user::unlink(join(dir, name, &mut gone));
    }
}

/// Shared writable file mappings: `READ` sees what they write, two mappings see each
/// other's writes, and what's written survives `MSYNC` and `MUNMAP` for a program started
/// after. In `dir`.
fn shared_checks(checks: &mut Checks, dir: &str) {
    let mut buffer = [0u8; 256];
    let path = join(dir, "mt-shared", &mut buffer);
    let made = make_file(path, 2, 0, pattern);
    let fd = open(path, flags::READ | flags::WRITE);
    let first = if fd >= 0 { mmap2_file(0, 2 * PAGE_SIZE, RW, map::SHARED, fd as usize, 0) } else { fd };
    let second = if fd >= 0 { mmap2_file(0, 2 * PAGE_SIZE, RW, map::SHARED, fd as usize, 0) } else { fd };
    if !made || first < 0 || second < 0 {
        checks.check("mmap-shared-setup", false, format_args!("made={made} fd={fd} maps={first:#x},{second:#x}"));
        return;
    }
    let (fd, first, second) = (fd as usize, first as usize, second as usize);

    // Written through the mapping, the file reads as written, before any write-back.
    write(first + 10, 0xa1);
    write(first + PAGE_SIZE + 20, 0xb2);
    let mut page = [0u8; 2 * PAGE_SIZE];
    feos_user::seek(fd, 0, seek::SET);
    let got = feos_user::read(fd, &mut page);
    let good = got == page.len() as isize && page[10] == 0xa1 && page[PAGE_SIZE + 20] == 0xb2 && page[11] == pattern(11);
    checks.check("mmap-shared-write-read", good, format_args!("read={got} bytes={:#x},{:#x}", page[10], page[PAGE_SIZE + 20]));

    // Two mappings of one file, at different addresses, are one copy.
    let seen = read(second + 10) == 0xa1 && read(second + PAGE_SIZE + 20) == 0xb2;
    write(second + 40, 0xd4);
    let back = read(first + 40) == 0xd4;
    write(first + 40, pattern(40));
    checks.check(
        "mmap-shared-two-mappings",
        first != second && seen && back,
        format_args!("first={first:#x} second={second:#x} seen={seen} back={back}"),
    );

    // Synced, a program started after reads it.
    let synced = msync(first, 2 * PAGE_SIZE, ms::SYNC);
    let status = child_in("shared-read-synced", dir);
    checks.check("mmap-shared-msync-persists", synced == 0 && status == 0, format_args!("msync={synced} child={status}"));

    // Unmapped without a sync, and the fd closed, likewise.
    write(second + 30, 0xc3);
    let gone = [munmap(first, 2 * PAGE_SIZE), munmap(second, 2 * PAGE_SIZE)];
    close(fd);
    let status = child_in("shared-read-unmapped", dir);
    checks.check(
        "mmap-shared-munmap-persists",
        gone == [0, 0] && status == 0,
        format_args!("munmap={gone:?} child={status}"),
    );

    // SQLite's `-shm` file: made 32 KiB of zeroes by writing them, mapped shared, written,
    // unmapped, and read back.
    let mut shm_buffer = [0u8; 256];
    let shm = join(dir, "mt-shm", &mut shm_buffer);
    let made = put_file(shm, &[0u8; 32 * 1024]);
    let fd = open(shm, flags::READ | flags::WRITE);
    let at = if made && fd >= 0 { mmap2_file(0, 32 * 1024, RW, map::SHARED, fd as usize, 0) } else { -1 };
    if at > 0 {
        for index in 0..8 {
            write(at as usize + index * PAGE_SIZE + index, 0x10 + index as u8);
        }
        munmap(at as usize, 32 * 1024);
    }
    if fd >= 0 {
        close(fd as usize);
    }
    let fd = open(shm, flags::READ);
    let mut wrong = 0;
    for index in 0..8 {
        let mut page = [0u8; PAGE_SIZE];
        let got = if fd >= 0 { feos_user::read(fd as usize, &mut page) } else { fd };
        let good = got == PAGE_SIZE as isize
            && page.iter().enumerate().all(|(offset, &byte)| byte == if offset == index { 0x10 + index as u8 } else { 0 });
        wrong += usize::from(!good);
    }
    if fd >= 0 {
        close(fd as usize);
    }
    checks.check("shm-file-pattern", at > 0 && wrong == 0, format_args!("at={at:#x} wrong-pages={wrong}"));
}

/// Writes `text` and a newline into `path` through a shared mapping, over a file of as
/// many dots, and syncs it: zero if every step worked.
fn persist(path: &str, text: &str) -> i32 {
    let length = text.len() + 1;
    if length > PAGE_SIZE || !put_file(path, &[b'.'; PAGE_SIZE][..length]) {
        println!("mtest persist: FAILED (making {path})");
        return 1;
    }
    let fd = open(path, flags::READ | flags::WRITE);
    let at = if fd >= 0 { mmap2_file(0, PAGE_SIZE, RW, map::SHARED, fd as usize, 0) } else { fd };
    if at < 0 {
        println!("mtest persist: FAILED (open {fd}, map {at})");
        return 1;
    }
    close(fd as usize);
    for (offset, &byte) in text.as_bytes().iter().chain(b"\n").enumerate() {
        write(at as usize + offset, byte);
    }
    let synced = msync(at as usize, PAGE_SIZE, ms::SYNC);
    let gone = munmap(at as usize, PAGE_SIZE);
    if synced == 0 && gone == 0 {
        println!("mtest persist: ok ({length} bytes written through a mapping and synced)");
        0
    } else {
        println!("mtest persist: FAILED (msync={synced} munmap={gone})");
        1
    }
}

/// Two copies of a program share its code: the second costs less than the first.
fn text_shared(checks: &mut Checks) {
    let before = free_frames();
    if before.is_none() {
        checks.check("program-text-shared", true, format_args!("skipped: no frame counts"));
        return;
    }
    // `cat` reading a pipe waits for as long as its write end is open.
    let mut pids = [0isize; 2];
    let mut writers = [0usize; 2];
    let mut costs = [0usize; 2];
    let mut last = before;
    for copy in 0..2 {
        let Ok((reader, writer)) = feos_user::pipe() else {
            checks.check("program-text-shared", false, format_args!("no pipe"));
            return;
        };
        pids[copy] = spawn_io("/bin/cat", &["/bin/cat"], reader, feos_user::abi::INHERIT);
        close(reader);
        writers[copy] = writer;
        feos_user::sleep_ms(200);
        let now = free_frames();
        costs[copy] = last.zip(now).map_or(0, |(last, now)| last.saturating_sub(now));
        last = now;
    }
    for copy in 0..2 {
        close(writers[copy]);
        if pids[copy] > 0 {
            wait(pids[copy] as usize);
        }
    }
    checks.check(
        "program-text-shared",
        pids.iter().all(|&pid| pid > 0) && costs[1] < costs[0] && costs[1] > 0,
        format_args!("first={} second={} frames", costs[0], costs[1]),
    );
}

/// Files totalling `mib` MiB, more than the machine has free, mapped at once and read
/// through, a page at a time, with each MiB let go behind; a promised page given its
/// frame at the end. Zero if every page read as it should and nothing failed.
fn cache_pressure(dir: &str, mib: usize) -> i32 {
    const FILE_MIB: usize = 16;
    let files = mib.div_ceil(FILE_MIB);
    let file_pages = FILE_MIB << 8;
    let mut fds = [0usize; 64];
    let mut maps = [0usize; 64];
    if files > fds.len() {
        return 2;
    }
    let byte = |file: usize, page: usize| (file * 31 + page * 7) as u8;
    // A promised region, made before the cache fills memory and touched after.
    let promised_length = 16 << 20;
    let promised = mmap2(0, promised_length, prot::READ | prot::WRITE, map::PRIVATE);
    if promised < 0 {
        println!("mtest cache-pressure: FAILED (promise {promised})");
        return 1;
    }
    for file in 0..files {
        let mut name = [0u8; 16];
        name[..8].copy_from_slice(b"mt-pres-");
        name[8] = b'a' + file as u8;
        let mut path_buffer = [0u8; 256];
        let path = join(dir, core::str::from_utf8(&name[..9]).unwrap_or("x"), &mut path_buffer);
        let existing = feos_user::stat(path).ok().map(|stat| stat.size);
        if existing != Some((file_pages * PAGE_SIZE) as u64) {
            let fd = open(path, flags::WRITE | flags::CREATE | flags::TRUNCATE);
            if fd < 0 {
                println!("mtest cache-pressure: FAILED (create {fd})");
                return 1;
            }
            let mut chunk = [0u8; 4096];
            for page in 0..file_pages {
                chunk.fill(byte(file, page));
                if write_fd(fd as usize, &chunk) != 4096 {
                    println!("mtest cache-pressure: FAILED (write of {path})");
                    return 1;
                }
            }
            close(fd as usize);
        }
        let fd = open(path, flags::READ);
        let at = mmap2_file(0, file_pages * PAGE_SIZE, prot::READ, map::PRIVATE, fd as usize, 0);
        if fd < 0 || at < 0 {
            println!("mtest cache-pressure: FAILED (open {fd}, map {at})");
            return 1;
        }
        fds[file] = fd as usize;
        maps[file] = at as usize;
    }
    let mut wrong = 0usize;
    let per_mib = 256;
    for file in 0..files {
        for page in 0..file_pages {
            let at = maps[file] + page * PAGE_SIZE;
            if read(at) != byte(file, page) || read(at + PAGE_SIZE - 1) != byte(file, page) {
                wrong += 1;
            }
            if page % per_mib == per_mib - 1 {
                madvise(at + PAGE_SIZE - per_mib * PAGE_SIZE, per_mib * PAGE_SIZE, madv::DONTNEED);
            }
        }
    }
    // Every promised page has its frame, however full the cache got.
    for page in 0..promised_length / PAGE_SIZE {
        write(promised as usize + page * PAGE_SIZE, page as u8);
    }
    let kept = (0..promised_length / PAGE_SIZE).all(|page| read(promised as usize + page * PAGE_SIZE) == page as u8);
    for file in 0..files {
        munmap(maps[file], file_pages * PAGE_SIZE);
        close(fds[file]);
    }
    munmap(promised as usize, promised_length);
    if wrong == 0 && kept {
        println!("mtest cache-pressure: ok ({files} files, {} MiB read through, promised pages kept)", files * FILE_MIB);
        0
    } else {
        println!("mtest cache-pressure: FAILED (wrong={wrong} kept={kept})");
        1
    }
}

fn main(args: Args) -> i32 {
    let dir = args.value_of("--dir").unwrap_or("/tmp");
    if let Some(name) = args.value_of("--child") {
        return run_child(name, dir, &args);
    }
    if let Some(path) = args.value_of("--ino") {
        return match feos_user::stat(path) {
            Ok(stat) => {
                println!("mtest ino {path}: {}", stat.ino);
                0
            }
            Err(error) => {
                println!("mtest ino {path}: {error}");
                1
            }
        };
    }
    if let Some(path) = args.value_of("--persist") {
        // The text is the word after the path.
        let text = (0..args.len()).find(|&at| args.get(at) == Some("--persist")).and_then(|at| args.get(at + 2)).unwrap_or("persisted");
        return persist(path, text);
    }
    if let Some(mib) = args.value_of("--cache-pressure") {
        return cache_pressure(dir, feos_user::parse_number(mib).unwrap_or(160));
    }
    feos_user::set_driven(args.has("--frames"));
    let mut checks = Checks { failed: 0 };
    let measured = free_frames().is_some();
    if !measured {
        println!("mtest: no frame counts (run as `ktest drive /bin/mtest`): those parts skipped");
    }

    // Private anonymous memory: zeroed, there to write, gone when unmapped.
    let length = 64 * PAGE_SIZE;
    let at = mmap2(0, length, RW, PRIVATE);
    let good = at > 0 && all(at as usize, length, 0);
    if good {
        for page in 0..64 {
            write(at as usize + page * PAGE_SIZE + page, page as u8 + 1);
        }
    }
    let kept = good && (0..64).all(|page| read(at as usize + page * PAGE_SIZE + page) == page as u8 + 1);
    let gone = munmap(at as usize, length);
    checks.check("mmap2-anon", kept && gone == 0, format_args!("at={at:#x} munmap={gone}"));

    // A hint that's free is where the mapping goes; one that's taken is passed over.
    let hint = AREA + 2 * GIB;
    let first = mmap2(hint + 123, 2 * PAGE_SIZE, RW, PRIVATE);
    checks.check("mmap2-hint", first == hint as isize, format_args!("hint={hint:#x} got={first:#x}"));
    let second = mmap2(hint, 2 * PAGE_SIZE, RW, PRIVATE);
    let apart = second > 0 && (second as usize + 2 * PAGE_SIZE <= hint || second as usize >= hint + 2 * PAGE_SIZE);
    checks.check("mmap2-hint-taken", apart, format_args!("got={second:#x}"));
    munmap(hint, 2 * PAGE_SIZE);
    munmap(second as usize, 2 * PAGE_SIZE);

    // `MAP_FIXED` replaces what's there, and only that.
    let at = mmap2(0, 4 * PAGE_SIZE, RW, PRIVATE) as usize;
    for page in 0..4 {
        write(at + page * PAGE_SIZE, 0xaa);
    }
    let fixed = mmap2(at + PAGE_SIZE, 2 * PAGE_SIZE, RW, PRIVATE | map::FIXED);
    let good = fixed == (at + PAGE_SIZE) as isize
        && read(at) == 0xaa
        && read(at + PAGE_SIZE) == 0
        && read(at + 2 * PAGE_SIZE) == 0
        && read(at + 3 * PAGE_SIZE) == 0xaa;
    checks.check("mmap2-fixed", good, format_args!("at={at:#x} fixed={fixed:#x}"));

    // `MAP_FIXED_NOREPLACE`: never over anything.
    let over = mmap2(at, PAGE_SIZE, RW, PRIVATE | map::FIXED_NOREPLACE);
    let free = at + 64 * PAGE_SIZE;
    let beside = mmap2(free, PAGE_SIZE, RW, PRIVATE | map::FIXED_NOREPLACE);
    checks.check(
        "mmap2-fixed-noreplace",
        over == -errno::EEXIST && beside == free as isize && read(at) == 0xaa,
        format_args!("over={over} beside={beside:#x}"),
    );
    munmap(at, 4 * PAGE_SIZE);
    munmap(free, PAGE_SIZE);

    // A reservation far over the limit costs nothing; made usable without a charge and
    // touched here and there, it costs the pages touched and their tables.
    let span = 8 * GIB;
    let before = free_frames();
    let reserved = mmap2(0, span, prot::NONE, PRIVATE | map::NORESERVE);
    let opened = if reserved > 0 { mprotect(reserved as usize, span, RW) } else { reserved };
    let pages = 100;
    let stride = (span / pages) & !(PAGE_SIZE - 1);
    let spot = |page: usize| reserved as usize + page * stride + page * 8;
    if opened == 0 {
        for page in 0..pages {
            write(spot(page), page as u8 + 1);
        }
    }
    let after = free_frames();
    let intact = opened == 0 && (0..pages).all(|page| read(spot(page)) == page as u8 + 1);
    let cost = before.zip(after).map(|(before, after)| before.saturating_sub(after));
    // Each page and, at worst, a leaf table each and a middle table a gigabyte.
    let cheap = cost.is_none_or(|cost| (pages..=2 * pages + 8).contains(&cost));
    // Without `MAP_NORESERVE`, making it all writable would promise it all: refused.
    let charged = mmap2(0, span, prot::NONE, PRIVATE);
    let refused = if charged > 0 { mprotect(charged as usize, span, RW) } else { charged };
    if charged > 0 {
        munmap(charged as usize, span);
    }
    munmap(reserved as usize, span);
    let back = free_frames();
    checks.check(
        "mmap2-noreserve-sparse",
        intact && cheap && refused == -errno::ENOMEM && before.zip(back).is_none_or(|(before, back)| back >= before),
        format_args!("reserved={reserved:#x} mprotect={opened} cost={cost:?} charged-mprotect={refused} frames={before:?}->{back:?}"),
    );

    // A reservation of no access builds nothing at all.
    let before = free_frames();
    let reserved = mmap2(0, 4 * GIB, prot::NONE, PRIVATE);
    let after = free_frames();
    munmap(reserved as usize, 4 * GIB);
    checks.check(
        "prot-none-is-free",
        reserved > 0 && before == after,
        format_args!("reserved={reserved:#x} frames={before:?}->{after:?}"),
    );

    // `MAP_POPULATE` gives every page its frame before it returns.
    let before = free_frames();
    let populated = mmap2(0, 64 * PAGE_SIZE, RW, PRIVATE | map::POPULATE);
    let after = free_frames();
    let zero = populated > 0 && all(populated as usize, 64 * PAGE_SIZE, 0);
    let paid = before.zip(after).is_none_or(|(before, after)| before >= after + 64);
    munmap(populated as usize, 64 * PAGE_SIZE);
    checks.check("mmap2-populate", zero && paid, format_args!("at={populated:#x} frames={before:?}->{after:?}"));

    // `MADV_DONTNEED`: the pages read as zeroes again, and their frames go.
    let at = mmap2(0, 4 * PAGE_SIZE, RW, PRIVATE) as usize;
    for page in 0..4 {
        write(at + page * PAGE_SIZE, 0xab);
    }
    let before = free_frames();
    let advised = madvise(at + PAGE_SIZE, 2 * PAGE_SIZE, madv::DONTNEED);
    let after = free_frames();
    let good = advised == 0
        && read(at) == 0xab
        && all(at + PAGE_SIZE, 2 * PAGE_SIZE, 0)
        && read(at + 3 * PAGE_SIZE) == 0xab
        && before.zip(after).is_none_or(|(before, after)| after >= before + 2);
    let others = [madv::NORMAL, madv::RANDOM, madv::SEQUENTIAL, madv::WILLNEED, madv::DONTFORK, madv::DODUMP]
        .iter()
        .all(|&advice| madvise(at, 4 * PAGE_SIZE, advice) == 0);
    let unknown = madvise(at, PAGE_SIZE, 5);
    let hole = madvise(at, 5 * PAGE_SIZE, madv::DONTNEED);
    checks.check(
        "madvise-dontneed-zeroes",
        good && others && unknown == -errno::EINVAL && hole == -errno::ENOMEM && read(at) == 0xab,
        format_args!("madvise={advised} unknown={unknown} hole={hole} frames={before:?}->{after:?}"),
    );
    munmap(at, 4 * PAGE_SIZE);

    // `MSYNC` checks: exactly one of async and sync, a mapped range. On anonymous memory
    // there's nothing to write back.
    let at = mmap2(0, 2 * PAGE_SIZE, RW, PRIVATE) as usize;
    let results = [
        msync(at, 2 * PAGE_SIZE, ms::SYNC),
        msync(at, 2 * PAGE_SIZE, ms::ASYNC | ms::INVALIDATE),
        msync(at, PAGE_SIZE, ms::SYNC | ms::ASYNC),
        msync(at, PAGE_SIZE, 0),
        msync(at, PAGE_SIZE, ms::INVALIDATE),
        msync(at, PAGE_SIZE, 8 | ms::SYNC),
        msync(at + 1, PAGE_SIZE, ms::SYNC),
        msync(at, 3 * PAGE_SIZE, ms::SYNC),
    ];
    let wanted = [0, 0, -errno::EINVAL, -errno::EINVAL, -errno::EINVAL, -errno::EINVAL, -errno::EINVAL, -errno::ENOMEM];
    checks.check("msync-validates", results == wanted, format_args!("{results:?}"));
    munmap(at, 2 * PAGE_SIZE);

    // `munmap` over two mappings and the hole between.
    let first = mmap2(0, 2 * PAGE_SIZE, RW, PRIVATE) as usize;
    let second = mmap2(0, 2 * PAGE_SIZE, RW, PRIVATE) as usize;
    let gone = munmap(first, second + 2 * PAGE_SIZE - first);
    let checked = [msync(first, PAGE_SIZE, ms::SYNC), msync(second, PAGE_SIZE, ms::SYNC)];
    checks.check(
        "munmap-hole",
        second > first + 2 * PAGE_SIZE && gone == 0 && checked == [-errno::ENOMEM; 2],
        format_args!("first={first:#x} second={second:#x} munmap={gone} after={checked:?}"),
    );

    // `munmap` from the middle leaves both ends.
    let at = mmap2(0, 3 * PAGE_SIZE, RW, PRIVATE) as usize;
    for page in 0..3 {
        write(at + page * PAGE_SIZE, 0x30 + page as u8);
    }
    let gone = munmap(at + PAGE_SIZE, PAGE_SIZE);
    let good = gone == 0
        && read(at) == 0x30
        && read(at + 2 * PAGE_SIZE) == 0x32
        && msync(at + PAGE_SIZE, PAGE_SIZE, ms::SYNC) == -errno::ENOMEM;
    let status = child("read-unmapped-middle");
    checks.check("munmap-splits", good && status == 139, format_args!("munmap={gone} child={status}"));
    munmap(at, 3 * PAGE_SIZE);

    // `mprotect` of the middle leaves both ends as they were.
    let at = mmap2(0, 3 * PAGE_SIZE, RW, PRIVATE) as usize;
    for page in 0..3 {
        write(at + page * PAGE_SIZE, 0x40 + page as u8);
    }
    let changed = mprotect(at + PAGE_SIZE, PAGE_SIZE, prot::READ);
    write(at, 0x50);
    write(at + 2 * PAGE_SIZE, 0x52);
    let good = changed == 0 && read(at) == 0x50 && read(at + PAGE_SIZE) == 0x41 && read(at + 2 * PAGE_SIZE) == 0x52;
    let status = child("write-read-only-middle");
    let none = child("read-prot-none");
    let hole = mprotect(at, 4 * PAGE_SIZE, prot::READ);
    checks.check(
        "mprotect-splits",
        good && status == 139 && none == 139 && hole == -errno::ENOMEM && read(at + 2 * PAGE_SIZE) == 0x52,
        format_args!("mprotect={changed} child={status} none-child={none} hole={hole}"),
    );
    munmap(at, 3 * PAGE_SIZE);

    // Shared anonymous memory: one page of an object, whichever mapping touches it.
    let at = mmap2(0, 2 * PAGE_SIZE, RW, map::SHARED);
    let good = at > 0 && {
        let at = at as usize;
        write(at, 0x61);
        madvise(at, PAGE_SIZE, madv::DONTNEED) == 0 && read(at) == 0x61 && read(at + PAGE_SIZE) == 0
    };
    munmap(at as usize, 2 * PAGE_SIZE);
    checks.check("mmap2-shared-anon", good, format_args!("at={at:#x}"));

    // v1's `MMAP` still promises every page, whatever its protection: two halves of the
    // limit and a gigabyte more of nothing are more than it allows (or, on a small
    // machine, than there is to promise), where `MMAP2` takes both.
    let half = USER_MEMORY_LIMIT / 2 + GIB;
    let first = mmap(half, prot::NONE);
    let second = mmap(half, prot::NONE);
    let v1_refused = first == -errno::ENOMEM || second == -errno::ENOMEM;
    for result in [first, second] {
        if result > 0 {
            munmap(result as usize, half);
        }
    }
    let first = mmap2(0, half, prot::NONE, PRIVATE);
    let second = mmap2(0, half, prot::NONE, PRIVATE);
    let v2_took = first > 0 && second > 0;
    munmap(first as usize, half);
    munmap(second as usize, half);
    // And a v1 page with no access made writable later can't newly fail.
    let guard = mmap(PAGE_SIZE, prot::NONE);
    let opened = mprotect(guard as usize, PAGE_SIZE, RW);
    if opened == 0 {
        write(guard as usize, 0x71);
    }
    let usable = opened == 0 && read(guard as usize) == 0x71;
    munmap(guard as usize, PAGE_SIZE);
    checks.check(
        "v1-mmap-unchanged",
        v1_refused && v2_took && usable,
        format_args!("v1-refused={v1_refused} v2-took={v2_took} v1-none-then-write={usable}"),
    );

    // The cap on mappings is the documented one, at least as seen from here.
    checks.check("vma-cap-constant", MAX_VMAS == 16384, format_args!("MAX_VMAS={MAX_VMAS}"));

    layout_checks(&mut checks, dir);

    file_checks(&mut checks, dir);
    text_shared(&mut checks);

    if measured {
        println!("mtest: frame counts checked");
    }
    if checks.failed == 0 {
        println!("mtest: all ok");
        0
    } else {
        println!("mtest: {} FAILED", checks.failed);
        1
    }
}

entry!(main);
