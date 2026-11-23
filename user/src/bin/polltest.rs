//! Runs through stage 1c.11: `POLL` on pipes, devices and the terminal, with a timeout,
//! a signal mask, many entries and a waker thread; and the terminal's `IOCTL`s, run on
//! the terminal the shell started it on: `isatty`, the window size, `FD_NREAD`, drain,
//! flush, the session, `/dev/tty`, a background group's read and `SPAWN2`'s
//! `TCSETPGRP`. Reports each check, and exits zero if all passed.
//!
//! `--raw-key` is for the boot test to drive: it puts the terminal in raw mode, says
//! `polltest raw: ready`, reads one key (no Enter) and reports `tcsetattr-raw`.
//!
//! `--flush` is the boot test's too: raw mode, `polltest flush: ready`, then it waits for
//! the three keys the test types, counts them with `FD_NREAD`, drops them with
//! `TCIFLUSH` and finds nothing left (`tcflush-input`); and has a write held by `TCOOFF`
//! until `TCOON` (`tcooff-holds-write`).
//!
//! Its children are itself, in one of the `--child` modes below, each reporting in its
//! exit status.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use feos_user::abi::{at, errno, ioctl as req, o, poll, sa, sig, spawn as spawn_flags, spawn_action, tc, wait, PollFd, SpawnAction, UserStr, WinSize};
use feos_user::signal::{block, handle, mask, set_disposition, unblock};
use feos_user::{
    close, entry, fstat, getpgid, getpid, getsid, ioctl, ioctl_int, isatty, kill, now_ns, openat, pipe, poll as poll_fds, println,
    read, seek, sleep_ms, tcgetattr, tcsetattr, tcsetpgrp, thread, wait4, winsize, write, Args,
};

const PROGRAM: &str = "/bin/polltest";

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, good: bool, detail: core::fmt::Arguments<'_>) {
        if good {
            println!("polltest {name}: ok ({detail})");
        } else {
            println!("polltest {name}: FAILED ({detail})");
            self.failed += 1;
        }
    }
}

const fn bit(signal: u32) -> u64 {
    1 << (signal - 1)
}

fn entry_of(fd: usize, events: i16) -> PollFd {
    PollFd { fd: fd as i32, events, revents: 0 }
}

static HITS: AtomicU32 = AtomicU32::new(0);

extern "C" fn count(_: i32, _: *mut feos_user::abi::SigInfo, _: *mut feos_user::abi::UContext) {
    HITS.fetch_add(1, Ordering::SeqCst);
}

fn test_pipes(report: &mut Report) {
    let Ok((reader, writer)) = pipe() else {
        report.check("poll-pipe-in-out", false, format_args!("no pipe"));
        return;
    };
    // Empty: the write end has room, the read end nothing.
    let mut fds = [entry_of(reader, poll::IN), entry_of(writer, poll::OUT)];
    let before = poll_fds(&mut fds, 0, None);
    let (read_before, write_before) = (fds[0].revents, fds[1].revents);
    write(writer, b"x");
    let mut fds = [entry_of(reader, poll::IN | poll::RDNORM), entry_of(writer, poll::OUT | poll::WRNORM)];
    let after = poll_fds(&mut fds, 0, None);
    report.check(
        "poll-pipe-in-out",
        before == 1 && read_before == 0 && write_before == poll::OUT && after == 2 && fds[0].revents == poll::IN | poll::RDNORM && fds[1].revents == poll::OUT | poll::WRNORM,
        format_args!("empty {before} ({read_before:#x}, {write_before:#x}), then {after} ({:#x}, {:#x})", fds[0].revents, fds[1].revents),
    );

    // The writer gone: `HUP` on the read end, asked for or not, and data still `IN`.
    close(writer);
    let mut fds = [entry_of(reader, 0)];
    let hup = poll_fds(&mut fds, 0, None);
    let hup_bits = fds[0].revents;
    let mut byte = [0u8; 1];
    read(reader, &mut byte);
    let mut fds = [entry_of(reader, poll::IN)];
    poll_fds(&mut fds, 0, None);
    let drained = fds[0].revents;
    // The reader gone: `ERR` on a write end.
    let Ok((reader2, writer2)) = pipe() else {
        return;
    };
    close(reader2);
    let mut fds = [entry_of(writer2, poll::OUT)];
    poll_fds(&mut fds, 0, None);
    let err = fds[0].revents;
    close(writer2);
    report.check(
        "poll-hup",
        hup == 1 && hup_bits == poll::HUP && drained == poll::IN | poll::HUP && err == poll::ERR,
        format_args!("no writer {hup_bits:#x}, drained {drained:#x}, no reader {err:#x}"),
    );

    // A descriptor that isn't open is `NVAL`; a negative one is skipped.
    let mut fds = [entry_of(reader, poll::IN), PollFd { fd: 900, events: poll::IN, revents: 0 }, PollFd { fd: -1, events: poll::IN, revents: 7 }];
    close(reader);
    let ready = poll_fds(&mut fds, 0, None);
    report.check(
        "poll-nval",
        ready == 2 && fds[0].revents == poll::NVAL && fds[1].revents == poll::NVAL && fds[2].revents == 0,
        format_args!("{ready} ready: {:#x} {:#x} {:#x}", fds[0].revents, fds[1].revents, fds[2].revents),
    );
}

fn test_timeout(report: &mut Report) {
    let Ok((reader, writer)) = pipe() else {
        return;
    };
    let mut fds = [entry_of(reader, poll::IN)];
    let start = now_ns(feos_user::abi::clock::MONOTONIC);
    let ready = poll_fds(&mut fds, 50_000_000, None);
    let took = (now_ns(feos_user::abi::clock::MONOTONIC) - start) / 1_000_000;
    report.check("poll-timeout", ready == 0 && fds[0].revents == 0 && (50..2000).contains(&took), format_args!("{ready} after {took} ms"));

    // A mask that unblocks a pending signal: `EINTR`, its handler run, the mask back.
    HITS.store(0, Ordering::SeqCst);
    handle(sig::USR1, count, 0);
    block(bit(sig::USR1));
    kill(getpid(), sig::USR1);
    let blocked = bit(sig::USR1);
    // With it still blocked, nothing comes of it.
    let kept = poll_fds(&mut fds, 0, Some(&blocked));
    let kept_hits = HITS.load(Ordering::SeqCst);
    let none = 0u64;
    let interrupted = poll_fds(&mut fds, 2_000_000_000, Some(&none));
    let hits = HITS.load(Ordering::SeqCst);
    let after = mask();
    unblock(bit(sig::USR1));
    set_disposition(sig::USR1, sa::DFL);
    report.check(
        "poll-sigmask",
        kept == 0 && kept_hits == 0 && interrupted == -errno::EINTR && hits == 1 && after == bit(sig::USR1),
        format_args!("kept {kept} ({kept_hits} hits), then {interrupted} with {hits} hits, mask after {after:#x}"),
    );
    close(reader);
    close(writer);
}

fn test_many(report: &mut Report) {
    // A process may have 64 pipes alive: 50 of them, each read end ten times over, is
    // 500 entries.
    const PIPES: usize = 50;
    const ENTRIES: usize = 500;
    let mut ends = [(0usize, 0usize); PIPES];
    for end in &mut ends {
        match pipe() {
            Ok(pair) => *end = pair,
            Err(error) => {
                report.check("poll-many", false, format_args!("pipe: {error}"));
                return;
            }
        }
    }
    let mut fds = [PollFd::default(); ENTRIES];
    for (index, entry) in fds.iter_mut().enumerate() {
        *entry = entry_of(ends[index % PIPES].0, poll::IN);
    }
    let idle = poll_fds(&mut fds, 0, None);
    write(ends[7].1, b"wake");
    let ready = poll_fds(&mut fds, 1_000_000_000, None);
    let right = fds.iter().enumerate().all(|(index, entry)| (entry.revents == poll::IN) == (index % PIPES == 7));
    report.check("poll-many", idle == 0 && ready == 10 && right, format_args!("{ENTRIES} entries: {idle} idle, {ready} ready"));

    // Bits nothing reports are a request, not an error.
    let mut fds = [entry_of(ends[7].0, poll::IN | poll::RDHUP | poll::WRBAND | poll::PRI | poll::MSG | poll::RDBAND)];
    let ready = poll_fds(&mut fds, 0, None);
    report.check("poll-unknown-events-accepted", ready == 1 && fds[0].revents == poll::IN, format_args!("{ready}: {:#x}", fds[0].revents));
    for (reader, writer) in ends {
        close(reader);
        close(writer);
    }
}

static WAKE_FD: AtomicUsize = AtomicUsize::new(0);

fn waker(_: usize) -> usize {
    sleep_ms(50);
    write(WAKE_FD.load(Ordering::SeqCst), b"!") as usize
}

fn test_waker(report: &mut Report) {
    let Ok((reader, writer)) = pipe() else {
        return;
    };
    WAKE_FD.store(writer, Ordering::SeqCst);
    let start = now_ns(feos_user::abi::clock::MONOTONIC);
    let other = thread::spawn(waker, 0);
    let mut fds = [entry_of(reader, poll::IN)];
    let ready = poll_fds(&mut fds, -1, None);
    let took = (now_ns(feos_user::abi::clock::MONOTONIC) - start) / 1_000_000;
    let wrote = other.map(thread::Thread::join).unwrap_or(0);
    report.check(
        "poll-woken-by-other-thread",
        ready == 1 && fds[0].revents == poll::IN && wrote == 1 && took < 2000,
        format_args!("{ready} after {took} ms"),
    );
    close(reader);
    close(writer);

    let null = openat(at::FDCWD, "/dev/null", o::RDWR, 0);
    let mut fds = [entry_of(null as usize, poll::IN | poll::OUT)];
    let ready = poll_fds(&mut fds, 0, None);
    report.check("poll-dev-null", null >= 0 && ready == 1 && fds[0].revents == poll::IN | poll::OUT, format_args!("{ready}: {:#x}", fds[0].revents));
    close(null as usize);
}

fn test_terminal(report: &mut Report) {
    let all = isatty(0) && isatty(1) && isatty(2);
    let mut fds = [entry_of(1, poll::OUT)];
    let ready = poll_fds(&mut fds, 0, None);
    report.check("isatty-console", all && ready == 1 && fds[0].revents == poll::OUT, format_args!("0-2 {all}, poll out {:#x}", fds[0].revents));
    if let Ok((reader, writer)) = pipe() {
        let attr = ioctl(reader, req::TTY_GET_ATTR, [0u8; 60].as_ptr() as usize);
        let sid = ioctl_int(writer, req::TTY_GET_SID);
        report.check("isatty-pipe-false", attr == -errno::ENOTTY && sid == Err(-errno::ENOTTY), format_args!("{attr} {sid:?}"));
        close(reader);
        close(writer);
    }

    // The size the window gave it; a new one is kept, with `SIGWINCH` for the foreground.
    let size = winsize(0);
    HITS.store(0, Ordering::SeqCst);
    handle(sig::WINCH, count, 0);
    let wanted = size.map(|size| WinSize { rows: size.rows + 1, columns: size.columns + 2, ..size });
    let set = wanted.map(|wanted| ioctl(1, req::TTY_SET_WINSIZE, wanted.to_bytes().as_ptr() as usize));
    let back = winsize(2);
    let hits = HITS.load(Ordering::SeqCst);
    if let Ok(size) = size {
        ioctl(1, req::TTY_SET_WINSIZE, size.to_bytes().as_ptr() as usize);
    }
    set_disposition(sig::WINCH, sa::DFL);
    report.check(
        "winsize",
        size.is_ok_and(|size| size.rows > 0 && size.columns > 0) && set == Ok(0) && back == wanted && hits == 1,
        format_args!("{:?} then {:?}, {hits} SIGWINCH", size.map(|size| (size.rows, size.columns)), back.map(|size| (size.rows, size.columns))),
    );

    // `FD_NREAD`: a pipe's bytes, a file's past the offset, the terminal's nothing yet.
    let mut piped = -1;
    if let Ok((reader, writer)) = pipe() {
        write(writer, b"12345");
        piped = ioctl_int(reader, req::FD_NREAD).unwrap_or(-1);
        close(reader);
        close(writer);
    }
    let file = openat(at::FDCWD, "/tmp/polltest-nread", o::RDWR | o::CREAT | o::TRUNC, 0o644);
    let mut in_file = -1;
    if file >= 0 {
        write(file as usize, b"0123456789");
        seek(file as usize, 3, 0);
        in_file = ioctl_int(file as usize, req::FD_NREAD).unwrap_or(-1);
        close(file as usize);
        feos_user::unlinkat(at::FDCWD, "/tmp/polltest-nread", 0);
    }
    let typed = ioctl_int(0, req::FD_NREAD);
    report.check("fionread", piped == 5 && in_file == 7 && typed.is_ok_and(|count| count >= 0), format_args!("pipe {piped}, file {in_file}, terminal {typed:?}"));

    println!("polltest: draining");
    let drained = ioctl(1, req::TTY_DRAIN, 0);
    report.check("tcdrain", drained == 0, format_args!("{drained}"));
    // Drained again first: an output flush drops what the shell hasn't shown yet, which
    // would be these reports.
    ioctl(1, req::TTY_DRAIN, 0);
    let flushed = [tc::IFLUSH, tc::OFLUSH, tc::IOFLUSH].map(|queue| ioctl(0, req::TTY_FLUSH, queue));
    let flow = [tc::OOFF, tc::OON, tc::IOFF, tc::ION].map(|action| ioctl(1, req::TTY_FLOW, action));
    report.check("tcflush", flushed == [0; 3] && flow == [0; 4], format_args!("flush {flushed:?}, flow {flow:?}"));

    let sid = ioctl_int(0, req::TTY_GET_SID);
    report.check("tcgetsid", sid.is_ok_and(|sid| sid as isize == getsid(0)), format_args!("{sid:?}, getsid {}", getsid(0)));

    // `/dev/tty` is the same terminal, opened anew.
    let tty = openat(at::FDCWD, "/dev/tty", o::RDWR, 0);
    let (same, device, wrote) = if tty >= 0 {
        let fd = tty as usize;
        let same = ioctl_int(fd, req::TTY_GET_SID) == sid && isatty(fd);
        let device = fstat(fd).map(|stat| stat.rdev).unwrap_or(0);
        let wrote = write(fd, b"polltest: through /dev/tty\n");
        close(fd);
        (same, device, wrote)
    } else {
        (false, 0, tty)
    };
    report.check("dev-tty-opens-terminal", tty >= 0 && same && device == 5 << 8 && wrote == 27, format_args!("fd {tty}, rdev {device:#x}, wrote {wrote}"));

    // A non-canonical read with `VMIN` 0 and `VTIME` 2: nothing typed, so nothing read,
    // after a fifth of a second.
    let saved = tcgetattr(0);
    let mut timed = -1;
    let mut took = 0;
    if let Ok(saved) = saved {
        let mut raw = saved;
        raw.lflag &= !(tc::ICANON | tc::ECHO);
        raw.cc[tc::VMIN] = 0;
        raw.cc[tc::VTIME] = 2;
        tcsetattr(0, req::TTY_SET_ATTR, &raw);
        let start = now_ns(feos_user::abi::clock::MONOTONIC);
        timed = read(0, &mut [0u8; 8]);
        took = (now_ns(feos_user::abi::clock::MONOTONIC) - start) / 1_000_000;
        tcsetattr(0, req::TTY_SET_ATTR, &saved);
    }
    let restored = tcgetattr(0);
    report.check(
        "vmin-vtime-read",
        timed == 0 && (150..3000).contains(&took) && restored == saved,
        format_args!("{timed} after {took} ms"),
    );
}

/// Starts this program as `mode` with `flags` and `actions`.
fn spawn_child(mode: &str, flags: u64, actions: &[SpawnAction]) -> isize {
    let mut argv = [UserStr::default(); 3];
    let argv = feos_user::user_strs(&[PROGRAM, "--child", mode], &mut argv);
    let mut request = feos_user::spawn_request(PROGRAM, argv, &[], actions);
    request.flags = flags;
    feos_user::spawn2(&request)
}

fn test_groups(report: &mut Report) {
    let me = getpgid(0);
    // A child in a group of its own is in the background: its read raises `SIGTTIN`,
    // which a handler takes and the read is `EINTR`; ignored, the read is `EIO`.
    let child = spawn_child("background-read", spawn_flags::SETPGROUP, &[]);
    let handled = if child > 0 { wait4(child, 0) } else { Err(child) };
    // With the default action, the read stops it.
    let stopper = spawn_child("background-stop", spawn_flags::SETPGROUP, &[]);
    let stopped = if stopper > 0 { wait4(stopper, wait::UNTRACED) } else { Err(stopper) };
    if stopper > 0 {
        kill(stopper, sig::KILL);
        let _ = wait4(stopper, 0);
    }
    let foreground = ioctl_int(0, req::TTY_GET_PGRP);
    report.check(
        "background-read-sigttin",
        handled.is_ok_and(|(_, status)| wait::exited(status) && wait::exit_code(status) == 0)
            && stopped.is_ok_and(|(_, status)| wait::stopped(status) && wait::stop_signal(status) == sig::TTIN as i32)
            && foreground == Ok(me as i32),
        format_args!("handled {handled:?}, default {stopped:?}, foreground {foreground:?}"),
    );

    // `TCSETPGRP` makes the child's new group the foreground one before it runs; this
    // program, in the background then, takes it back.
    let actions = [SpawnAction { kind: spawn_action::TCSETPGRP, fd: 0, ..SpawnAction::default() }];
    let child = spawn_child("foreground-check", spawn_flags::SETPGROUP, &actions);
    let status = if child > 0 { wait4(child, 0) } else { Err(child) };
    let during = ioctl_int(0, req::TTY_GET_PGRP);
    // Its group has gone with it: no foreground to make of it.
    let gone = tcsetpgrp(0, child as i32);
    let back = tcsetpgrp(0, me as i32);
    let after = ioctl_int(0, req::TTY_GET_PGRP);
    // Not on something that isn't a terminal, nor with a new session.
    let not_tty = [SpawnAction { kind: spawn_action::TCSETPGRP, fd: 900, ..SpawnAction::default() }];
    let refused = spawn_child("foreground-check", spawn_flags::SETPGROUP, &not_tty);
    let session = spawn_child("foreground-check", spawn_flags::SETSID, &actions);
    report.check(
        "spawn2-tcsetpgrp",
        status.is_ok_and(|(_, status)| wait::exited(status) && wait::exit_code(status) == 0)
            && during == Ok(child as i32)
            && gone == -errno::EPERM
            && back == 0
            && after == Ok(me as i32)
            && refused == -errno::EBADF
            && session == -errno::EPERM,
        format_args!("child {child} {status:?}, foreground {during:?}, gone {gone}, back {back} {after:?}, refused {refused} {session}"),
    );
}

/// `--child background-read`: in a background group, a read with `SIGTTIN` caught and
/// then ignored.
fn child_background_read() -> i32 {
    HITS.store(0, Ordering::SeqCst);
    handle(sig::TTIN, count, 0);
    let caught = read(0, &mut [0u8; 4]);
    let hits = HITS.load(Ordering::SeqCst);
    set_disposition(sig::TTIN, sa::IGN);
    let ignored = read(0, &mut [0u8; 4]);
    match (caught == -errno::EINTR && hits == 1, ignored == -errno::EIO) {
        (true, true) => 0,
        (false, _) => 1,
        (true, false) => 2,
    }
}

/// `--child foreground-check`: its own group is the terminal's foreground one.
fn child_foreground_check() -> i32 {
    let pgid = getpgid(0);
    let foreground = ioctl_int(0, req::TTY_GET_PGRP);
    i32::from(pgid != getpid() || foreground != Ok(pgid as i32))
}

/// `--raw-key`: raw mode, and one key.
fn raw_key() -> i32 {
    let mut report = Report { failed: 0 };
    let Ok(saved) = tcgetattr(0) else {
        report.check("tcsetattr-raw", false, format_args!("not a terminal"));
        return 1;
    };
    let mut raw = saved;
    raw.lflag &= !(tc::ICANON | tc::ECHO);
    raw.cc[tc::VMIN] = 1;
    raw.cc[tc::VTIME] = 0;
    let set = tcsetattr(0, req::TTY_SET_ATTR_FLUSH, &raw);
    let now = tcgetattr(0);
    println!("polltest raw: ready");
    let mut key = [0u8; 8];
    let got = read(0, &mut key);
    tcsetattr(0, req::TTY_SET_ATTR_DRAIN, &saved);
    report.check(
        "tcsetattr-raw",
        set == 0 && now == Ok(raw) && got == 1 && tcgetattr(0) == Ok(saved),
        format_args!("read {got}: {}", if got > 0 { key[0] } else { 0 }),
    );
    i32::from(report.failed > 0)
}

static HELD_DONE: AtomicU32 = AtomicU32::new(0);

/// A write that `TCOOFF` holds: its result, once `TCOON` lets it through.
fn held_writer(_: usize) -> usize {
    let wrote = write(1, b"polltest: held write through\n");
    HELD_DONE.store(1, Ordering::SeqCst);
    wrote as usize
}

/// `--flush`: typed input counted and dropped, and output held, in the kernel's terminal.
fn flush() -> i32 {
    const TYPED: i32 = 3;
    let mut report = Report { failed: 0 };
    let Ok(saved) = tcgetattr(0) else {
        report.check("tcflush-input", false, format_args!("not a terminal"));
        return 1;
    };
    // `VMIN` above what's typed: a reader that wants more keeps asking the host, so all
    // three come in and stay unread.
    let mut raw = saved;
    raw.lflag &= !(tc::ICANON | tc::ECHO);
    raw.cc[tc::VMIN] = 8;
    raw.cc[tc::VTIME] = 0;
    tcsetattr(0, req::TTY_SET_ATTR_FLUSH, &raw);
    println!("polltest flush: ready");
    // A poll that finds too little is a reader wanting input, so the host feeds it.
    let start = now_ns(feos_user::abi::clock::MONOTONIC);
    let mut typed = ioctl_int(0, req::FD_NREAD);
    while typed.is_ok_and(|count| count < TYPED) && now_ns(feos_user::abi::clock::MONOTONIC) - start < 20_000_000_000 {
        let mut fds = [entry_of(0, poll::IN)];
        poll_fds(&mut fds, 100_000_000, None);
        sleep_ms(20);
        typed = ioctl_int(0, req::FD_NREAD);
    }
    let flushed = ioctl(0, req::TTY_FLUSH, tc::IFLUSH);
    let after = ioctl_int(0, req::FD_NREAD);
    // Nothing left for a read that won't wait.
    raw.cc[tc::VMIN] = 0;
    tcsetattr(0, req::TTY_SET_ATTR, &raw);
    let left = read(0, &mut [0u8; 8]);
    tcsetattr(0, req::TTY_SET_ATTR, &saved);
    report.check(
        "tcflush-input",
        typed == Ok(TYPED) && flushed == 0 && after == Ok(0) && left == 0,
        format_args!("typed {typed:?}, flush {flushed}, then {after:?}, read {left}"),
    );

    // Output held: a non-blocking write is `EAGAIN`, a blocking one waits for `TCOON`.
    // Nothing else is written meanwhile, which would wait too.
    ioctl(1, req::TTY_DRAIN, 0);
    let tty = openat(at::FDCWD, "/dev/tty", o::RDWR | o::NONBLOCK, 0);
    HELD_DONE.store(0, Ordering::SeqCst);
    let off = ioctl(1, req::TTY_FLOW, tc::OOFF);
    let refused = if tty >= 0 { write(tty as usize, b"x") } else { tty };
    let writer = thread::spawn(held_writer, 0);
    sleep_ms(300);
    let held = HELD_DONE.load(Ordering::SeqCst) == 0;
    let on = ioctl(1, req::TTY_FLOW, tc::OON);
    let wrote = writer.map(thread::Thread::join).unwrap_or(0);
    if tty >= 0 {
        close(tty as usize);
    }
    report.check(
        "tcooff-holds-write",
        off == 0 && refused == -errno::EAGAIN && held && on == 0 && wrote == 29,
        format_args!("off {off}, non-blocking {refused}, held {held}, on {on}, then wrote {wrote}"),
    );
    i32::from(report.failed > 0)
}

fn main(args: Args) -> i32 {
    match args.value_of("--child") {
        Some("background-read") => return child_background_read(),
        Some("background-stop") => {
            read(0, &mut [0u8; 4]);
            return 3;
        }
        Some("foreground-check") => return child_foreground_check(),
        Some(_) => return 2,
        None => {}
    }
    if args.has("--raw-key") {
        return raw_key();
    }
    if args.has("--flush") {
        return flush();
    }
    let mut report = Report { failed: 0 };
    test_pipes(&mut report);
    test_timeout(&mut report);
    test_many(&mut report);
    test_waker(&mut report);
    test_terminal(&mut report);
    test_groups(&mut report);
    if report.failed == 0 {
        println!("polltest: all ok");
        0
    } else {
        println!("polltest: {} failed", report.failed);
        1
    }
}

entry!(main);
