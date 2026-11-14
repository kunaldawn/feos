//! Runs through the system calls and reports each one. Exits zero if all passed.

#![no_std]
#![no_main]

use core::sync::atomic::AtomicU32;

use feos_user::abi::{self, errno, open as flags, prot, seek as whence};
use feos_user::{
    close, entry, futex_wait, futex_wake, getpid, mmap, mprotect, munmap, open, pipe, println, read, seek,
    sleep_ms, spawn, spawn_io, uptime_ms, wait, write, Args,
};

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, good: bool) {
        if good {
            println!("utest {name} ok");
        } else {
            println!("utest {name} FAILED");
            self.failed += 1;
        }
    }
}

fn test_memory(report: &mut Report) {
    let rw = prot::READ | prot::WRITE;
    let first = mmap(3 * abi::PAGE_SIZE, rw);
    let good = first > 0 && {
        let cells = first as usize as *mut u8;
        // SAFETY: three pages just mapped.
        unsafe {
            cells.add(2 * abi::PAGE_SIZE).write_volatile(7);
            cells.read_volatile() == 0 && cells.add(2 * abi::PAGE_SIZE).read_volatile() == 7
        }
    };
    report.check("mmap-is-zeroed-and-writable", good);
    report.check("munmap", munmap(first as usize, 3 * abi::PAGE_SIZE) == 0);

    // Up to the limit, which is the kernel's to keep, and back. Past the process's
    // limit, so whichever of it and the machine's comes first.
    let mut mapped = [0usize; 160];
    let mut count = 0;
    let chunk = 64 * 1024 * 1024;
    while count < mapped.len() {
        let address = mmap(chunk, rw);
        if address < 0 {
            report.check("mmap-limit-is-enomem", address == -errno::ENOMEM);
            break;
        }
        mapped[count] = address as usize;
        count += 1;
    }
    println!("utest mapped {count} chunks of 64 MiB before the limit");
    report.check("mmap-limit-reached", count > 0 && count < mapped.len());
    let mut all = true;
    for address in mapped[..count].iter().rev() {
        all &= munmap(*address, chunk) == 0;
    }
    report.check("munmap-after-limit", all);
    let again = mmap(chunk, rw);
    report.check("memory-comes-back", again > 0);
    munmap(again as usize, chunk);
}

/// Memory that only gets frames when it's touched, and protection changes.
fn test_lazy_memory(report: &mut Report) {
    let rw = prot::READ | prot::WRITE;
    // As much as the machine will promise, up to a gibibyte, of which only a little is
    // touched.
    let mut length = 1024 * 1024 * 1024;
    let mut big = mmap(length, rw);
    while big == -errno::ENOMEM && length > 16 * 1024 * 1024 {
        length /= 2;
        big = mmap(length, rw);
    }
    println!("utest lazily mapped {} MiB", length / (1024 * 1024));
    let mut good = big > 0;
    if good {
        let base = big as usize as *mut u8;
        for step in [0, 4096 * 3 + 7, length / 2, length - 1] {
            // SAFETY: inside the mapping.
            unsafe {
                good &= base.add(step).read_volatile() == 0;
                base.add(step).write_volatile(0x5a);
                good &= base.add(step).read_volatile() == 0x5a;
            }
        }
        // The kernel commits what it's given to fill, too.
        let fd = open("/etc/motd", flags::READ);
        if fd >= 0 {
            let got = read(fd as usize, unsafe { core::slice::from_raw_parts_mut(base.add(length / 4), 64) });
            good &= got > 0;
            close(fd as usize);
        }
        good &= munmap(big as usize, length) == 0;
    }
    report.check("lazy-mmap-of-a-gibibyte", good);

    let page = mmap(2 * abi::PAGE_SIZE, rw);
    let mut good = page > 0;
    if good {
        let cell = page as usize as *mut u32;
        // SAFETY: just mapped, writable.
        unsafe { cell.write_volatile(0xfeed) };
        good &= mprotect(page as usize, 2 * abi::PAGE_SIZE, prot::READ) == 0;
        // SAFETY: still readable.
        good &= unsafe { cell.read_volatile() } == 0xfeed;
        good &= mprotect(page as usize, 2 * abi::PAGE_SIZE, 0) == 0;
        good &= mprotect(page as usize, 2 * abi::PAGE_SIZE, rw) == 0;
        // SAFETY: writable again, and kept what it had.
        good &= unsafe { cell.read_volatile() } == 0xfeed;
        unsafe { cell.add(abi::PAGE_SIZE / 4).write_volatile(1) };
        // The kernel won't write into a page the program can't.
        good &= mprotect(page as usize, abi::PAGE_SIZE, prot::READ) == 0;
        let fd = open("/etc/motd", flags::READ);
        if fd >= 0 {
            let got = read(fd as usize, unsafe { core::slice::from_raw_parts_mut(page as usize as *mut u8, 16) });
            good &= got == -errno::EFAULT;
            close(fd as usize);
        }
        // Not past the end, and not over a hole.
        good &= mprotect(page as usize, 64 * abi::PAGE_SIZE, prot::READ) == -errno::EINVAL;
        good &= munmap(page as usize, 2 * abi::PAGE_SIZE) == 0;
    }
    report.check("mprotect", good);

    let guard = mmap(abi::PAGE_SIZE, 0);
    report.check("mmap-no-access", guard > 0 && munmap(guard as usize, abi::PAGE_SIZE) == 0);
}

fn test_files(report: &mut Report) {
    let path = "/tmp/utest.txt";
    let fd = open(path, flags::READ | flags::WRITE | flags::CREATE | flags::TRUNCATE);
    report.check("open-create", fd >= 0);
    if fd < 0 {
        return;
    }
    let fd = fd as usize;
    report.check("file-write", write(fd, b"hello file") == 10);
    report.check("file-seek", seek(fd, 6, whence::SET) == 6);
    let mut buffer = [0u8; 16];
    let got = read(fd, &mut buffer);
    report.check("file-read", got == 4 && &buffer[..4] == b"file");
    report.check("file-seek-end", seek(fd, 0, whence::END) == 10);
    report.check("file-eof", read(fd, &mut buffer) == 0);
    report.check("close", close(fd) == 0);
    report.check("close-twice", close(fd) == -errno::EBADF);

    let fd = open(path, flags::WRITE | flags::APPEND);
    report.check("open-append", fd >= 0);
    write(fd as usize, b"!");
    close(fd as usize);
    let fd = open(path, flags::READ);
    let got = read(fd as usize, &mut buffer);
    report.check("append-landed", got == 11 && buffer[10] == b'!');
    report.check("write-to-read-only-fd", write(fd as usize, b"x") == -errno::EBADF);
    close(fd as usize);
    report.check("open-missing", open("/no/such/file", flags::READ) == -errno::ENOENT);
    report.check("open-directory", open("/etc", flags::READ) == -errno::EISDIR);
    report.check("open-no-mode", open(path, 0) == -errno::EINVAL);
}

fn test_processes(report: &mut Report) {
    let child = spawn("/bin/hello", &["/bin/hello", "--quiet", "--exit", "42"]);
    report.check("spawn", child > 0);
    report.check("wait-status", wait(child as usize) == 42);
    report.check("wait-again-is-the-same", wait(child as usize) == 42);
    report.check("wait-not-a-child", wait(1) == -errno::ECHILD);
    report.check("wait-nonsense", wait(usize::MAX) == -errno::ECHILD);

    let missing = spawn("/no/such/program", &["x"]);
    report.check("spawn-missing-ends-127", missing > 0 && wait(missing as usize) == 127);
    let not_elf = spawn("/etc/motd", &["x"]);
    report.check("spawn-not-elf-ends-126", not_elf > 0 && wait(not_elf as usize) == 126);

    // A child writes into this process through a pipe.
    if let Ok((reader, writer)) = pipe() {
        let child = spawn_io(
            "/bin/hello",
            &["/bin/hello", "through-a-pipe"],
            abi::INHERIT,
            writer,
        );
        close(writer);
        let mut buffer = [0u8; 256];
        let mut total = 0;
        loop {
            let got = read(reader, &mut buffer[total..]);
            if got <= 0 {
                break;
            }
            total += got as usize;
        }
        close(reader);
        let text = core::str::from_utf8(&buffer[..total]).unwrap_or("");
        report.check("child-stdout-pipe", text.contains("through-a-pipe"));
        report.check("child-status-through-pipe", wait(child as usize) == 0);
    } else {
        report.check("pipe", false);
    }

    // And through a child in both directions: this writes, /bin/cat copies.
    if let (Ok((in_read, in_write)), Ok((out_read, out_write))) = (pipe(), pipe()) {
        let child = spawn_io("/bin/cat", &["/bin/cat"], in_read, out_write);
        close(in_read);
        close(out_write);
        write(in_write, b"copied by a process\n");
        close(in_write);
        let mut buffer = [0u8; 64];
        let got = read(out_read, &mut buffer);
        close(out_read);
        report.check("child-stdin-pipe", got > 0 && buffer[..got as usize].starts_with(b"copied"));
        report.check("cat-sees-eof", wait(child as usize) == 0);
    } else {
        report.check("two-pipes", false);
    }

    if let Ok((reader, writer)) = pipe() {
        close(reader);
        report.check("write-with-no-reader", write(writer, b"x") == -errno::EPIPE);
        close(writer);
    }
}

/// Takes all the memory it's allowed, and holds it for a moment. Enough of these run the
/// machine out of frames, which has to be something a program hears about, not the kernel.
fn hog() -> i32 {
    let chunk = 4 * 1024 * 1024;
    let mut total = 0;
    loop {
        let address = mmap(chunk, prot::READ | prot::WRITE);
        if address == -errno::ENOMEM {
            break;
        }
        if address < 0 {
            return 1;
        }
        total += chunk;
    }
    println!("utest hog pid {} held {} MiB", getpid(), total / (1024 * 1024));
    sleep_ms(1500);
    0
}

fn test_exhaustion(report: &mut Report) {
    let mut children = [0isize; 8];
    for child in children.iter_mut() {
        *child = spawn("/bin/utest", &["/bin/utest", "--hog"]);
    }
    sleep_ms(1000);
    // With the machine full, a program that has to load fails to, and says so.
    let late = spawn("/bin/hello", &["/bin/hello", "--quiet"]);
    let status = if late > 0 { wait(late as usize) } else { -1 };
    println!("utest a program started when memory was short ended with {status}");
    report.check("short-of-memory-is-an-answer", status == 0 || status == 126);
    // Hogs that start late can find the machine already promised away and fail to load
    // (126): `mmap` only promises frames, so the first hogs fill memory before the last
    // are loaded. Either is an answer. Anything else (a crash, a kill) is not, and at
    // least one hog must have run, or nothing here was short of memory.
    let mut all = true;
    let mut ran = 0;
    for child in children {
        let status = if child > 0 { wait(child as usize) } else { -1 };
        all &= status == 0 || status == 126;
        ran += usize::from(status == 0);
    }
    println!("utest {ran} of {} hogs ran", children.len());
    report.check("everyone-short-of-memory-survived", all && ran > 0);
    let after = mmap(32 * 1024 * 1024, prot::READ | prot::WRITE);
    report.check("memory-came-back-after-exhaustion", after > 0);
    if after > 0 {
        munmap(after as usize, 32 * 1024 * 1024);
    }
}

fn test_time_and_futex(report: &mut Report) {
    let before = uptime_ms();
    sleep_ms(30);
    let slept = uptime_ms() - before;
    report.check("sleep", (29..2000).contains(&slept));
    report.check("getpid", getpid() > 1);

    let word = AtomicU32::new(5);
    report.check("futex-wrong-value", futex_wait(&word, 6, 0) == -errno::EAGAIN);
    let before = uptime_ms();
    report.check("futex-timeout", futex_wait(&word, 5, 30) == -errno::ETIMEDOUT);
    report.check("futex-timeout-waited", uptime_ms() - before >= 29);
    report.check("futex-wake-nobody", futex_wake(&word, 10) == 0);
}

fn main(args: Args) -> i32 {
    if args.has("--hog") {
        return hog();
    }
    let mut report = Report { failed: 0 };
    report.check("argv", args.get(0).is_some_and(|name| name.ends_with("utest")));
    test_memory(&mut report);
    test_lazy_memory(&mut report);
    test_files(&mut report);
    test_processes(&mut report);
    test_time_and_futex(&mut report);
    test_exhaustion(&mut report);
    if report.failed == 0 {
        println!("utest: all ok");
        0
    } else {
        println!("utest: {} FAILED", report.failed);
        1
    }
}

entry!(main);
