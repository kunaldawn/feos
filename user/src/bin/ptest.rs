//! Runs through starting programs (stage 1c.6): `SPAWN2`'s arguments, environment and
//! actions, its synchronous errors, process groups and sessions, the initial stack's aux
//! vector, `SYSINFO` and `GETRUSAGE`. Reports each check, and exits zero if all passed.
//!
//! The boot test runs it as `FOO=bar /bin/ptest`, so its own environment comes from the
//! shell. Its children are itself, in one of the modes below, each reporting what it
//! found on its standard output (a pipe the check reads) or in its exit status.

#![no_std]
#![no_main]

use feos_user::abi::{aux, errno, o, rusage, spawn, spawn_action, SpawnAction, SpawnRequest, UserStr, PAGE_SIZE};
use feos_user::{
    c_arg, close, entry, environment, getenv, getpid, getppid, getpgid, getsid, pipe2, println, read, setpgid, spawn2,
    spawn_request, syscall, user_strs, wait, write, Args,
};

const PROGRAM: &str = "/bin/ptest";

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, good: bool, detail: core::fmt::Arguments<'_>) {
        if good {
            println!("ptest {name}: ok ({detail})");
        } else {
            println!("ptest {name}: FAILED ({detail})");
            self.failed += 1;
        }
    }
}

/// What a child wrote, up to its end.
struct Captured {
    bytes: [u8; 512],
    length: usize,
}

impl Captured {
    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("?").trim_end()
    }
}

fn dup2(from: usize, to: usize) -> SpawnAction {
    SpawnAction { kind: spawn_action::DUP2, fd: from as i32, to: to as i32, ..SpawnAction::default() }
}

fn close_action(fd: i32) -> SpawnAction {
    SpawnAction { kind: spawn_action::CLOSE, fd, ..SpawnAction::default() }
}

/// Runs `request`, with `actions` and then its standard output sent to a pipe this
/// reads. The pid (or error), the status, and what it wrote.
fn capture(mut request: SpawnRequest, actions: &[SpawnAction]) -> (isize, isize, Captured) {
    let mut captured = Captured { bytes: [0; 512], length: 0 };
    let Ok((read_end, write_end)) = pipe2(o::CLOEXEC) else {
        return (-1, -1, captured);
    };
    let mut all = [SpawnAction::default(); 8];
    all[..actions.len()].copy_from_slice(actions);
    all[actions.len()] = dup2(write_end, 1);
    request.actions = all.as_ptr() as u64;
    request.action_count = actions.len() as u64 + 1;
    let pid = spawn2(&request);
    close(write_end);
    if pid > 0 {
        while captured.length < captured.bytes.len() {
            let got = read(read_end, &mut captured.bytes[captured.length..]);
            if got <= 0 {
                break;
            }
            captured.length += got as usize;
        }
    }
    close(read_end);
    let status = if pid > 0 { wait(pid as usize) } else { pid };
    (pid, status, captured)
}

/// A request for this program in `mode`, with `extra` arguments after it.
fn child(argv: &mut [UserStr; 8], words: &[&str]) -> SpawnRequest {
    let strings = user_strs(words, argv);
    spawn_request(PROGRAM, strings, &[], &[])
}

fn test_argv_envp(report: &mut Report) {
    let mut argv = [UserStr::default(); 8];
    let mut envp = [UserStr::default(); 8];
    let words = [PROGRAM, "--echo", "two words", ""];
    let env = ["A=1", "B=x y", "EMPTY="];
    let request = spawn_request(PROGRAM, user_strs(&words, &mut argv), user_strs(&env, &mut envp), &[]);
    let (pid, status, out) = capture(request, &[]);
    let wanted = "argv=/bin/ptest|--echo|two words||\nenv=A=1|B=x y|EMPTY=|";
    report.check("spawn2-argv-envp", status == 0 && out.text() == wanted, format_args!("pid={pid} status={status} {:?}", out.text()));
}

fn test_env_from_shell(report: &mut Report) {
    let value = getenv("FOO");
    let count = environment().count();
    report.check(
        "spawn2-env-from-shell",
        value == Some(b"bar".as_slice()) && getenv("PATH").is_some(),
        format_args!("FOO={:?} {count} variables", value.map(|value| core::str::from_utf8(value).unwrap_or("?"))),
    );
}

fn test_dup2(report: &mut Report) {
    let mut argv = [UserStr::default(); 8];
    let request = child(&mut argv, &[PROGRAM, "--write-fd", "5", "through five"]);
    // The pipe the capture makes goes to 5 as well as 1, and the child writes to 5.
    let Ok((read_end, write_end)) = pipe2(o::CLOEXEC) else {
        return report.check("spawn2-dup2-action", false, format_args!("no pipe"));
    };
    let mut request = request;
    let actions = [dup2(write_end, 5)];
    request.actions = actions.as_ptr() as u64;
    request.action_count = 1;
    let pid = spawn2(&request);
    close(write_end);
    let mut buffer = [0u8; 64];
    let got = if pid > 0 { read(read_end, &mut buffer) } else { pid };
    close(read_end);
    let status = if pid > 0 { wait(pid as usize) } else { pid };
    let text = core::str::from_utf8(&buffer[..got.max(0) as usize]).unwrap_or("?");
    report.check("spawn2-dup2-action", status == 0 && text == "through five", format_args!("status={status} {text:?}"));
}

/// Opens a file that stays open in this process, not close-on-exec unless `cloexec`.
fn open_file(cloexec: bool) -> usize {
    let flags = o::RDONLY | if cloexec { o::CLOEXEC } else { 0 };
    feos_user::openat(feos_user::abi::at::FDCWD, "/etc/motd", flags, 0) as usize
}

/// Runs this program with `--fd-state` and `specs` after `actions`; its status.
fn fd_state(specs: &[&str], actions: &[SpawnAction]) -> isize {
    let mut words = [""; 8];
    words[0] = PROGRAM;
    words[1] = "--fd-state";
    words[2..2 + specs.len()].copy_from_slice(specs);
    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, &words[..2 + specs.len()]);
    request.actions = actions.as_ptr() as u64;
    request.action_count = actions.len() as u64;
    let pid = spawn2(&request);
    if pid > 0 {
        wait(pid as usize)
    } else {
        pid
    }
}

/// A descriptor number as text, in `buffer`.
fn number(value: usize, buffer: &mut [u8; 24], prefix: u8) -> &str {
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut rest = value;
    loop {
        digits[count] = b'0' + (rest % 10) as u8;
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    buffer[0] = prefix;
    for index in 0..count {
        buffer[1 + index] = digits[count - 1 - index];
    }
    core::str::from_utf8(&buffer[..1 + count]).unwrap_or("")
}

fn test_close_actions(report: &mut Report) {
    let fd = open_file(false);
    let mut text = [0u8; 24];
    let closed = number(fd, &mut text, b'c');
    let status = fd_state(&[closed], &[close_action(fd as i32)]);
    report.check("spawn2-close-action", status == 0, format_args!("fd {fd} closed in the child: status {status}"));
    let mut text = [0u8; 24];
    let open = number(fd, &mut text, b'o');
    let status = fd_state(&[open], &[close_action(900)]);
    report.check("spawn2-close-not-open-ignored", status == 0, format_args!("close(900) ignored: status {status}"));
    let status = fd_state(&[open], &[]);
    report.check("spawn2-inherits-fds", status == 0, format_args!("fd {fd} open in the child: status {status}"));
    close(fd);

    // Descriptors from 20 up close; 10 stays.
    let base = open_file(false);
    let low = feos_user::dup3(base, 10, 0) as usize;
    let high = feos_user::dup3(base, 20, 0) as usize;
    let higher = feos_user::dup3(base, 30, 0) as usize;
    close(base);
    let closefrom = SpawnAction { kind: spawn_action::CLOSEFROM, fd: 15, ..SpawnAction::default() };
    let status = fd_state(&["o10", "c20", "c30"], &[closefrom]);
    report.check("spawn2-closefrom", (low, high, higher) == (10, 20, 30) && status == 0, format_args!("status {status}"));
    for fd in [low, high, higher] {
        close(fd);
    }

    let fd = open_file(true);
    let mut text = [0u8; 24];
    let closed = number(fd, &mut text, b'c');
    let status = fd_state(&[closed], &[]);
    report.check("spawn2-cloexec-closed", status == 0, format_args!("fd {fd} is O_CLOEXEC: status {status}"));
    close(fd);
}

fn test_open_actions(report: &mut Report) {
    let path = "/tmp/ptest-opened";
    let open = SpawnAction {
        kind: spawn_action::OPEN,
        fd: 6,
        open_flags: (o::WRONLY | o::CREAT | o::TRUNC) as u32,
        mode: 0o644,
        path: feos_user::path_at(feos_user::abi::at::FDCWD, path),
        ..SpawnAction::default()
    };
    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, &[PROGRAM, "--write-fd", "6", "opened"]);
    let actions = [open];
    request.actions = actions.as_ptr() as u64;
    request.action_count = 1;
    let pid = spawn2(&request);
    let status = if pid > 0 { wait(pid as usize) } else { pid };
    let fd = feos_user::openat(feos_user::abi::at::FDCWD, path, o::RDONLY, 0);
    let mut buffer = [0u8; 32];
    let got = if fd >= 0 { read(fd as usize, &mut buffer) } else { fd };
    if fd >= 0 {
        close(fd as usize);
    }
    feos_user::unlinkat(feos_user::abi::at::FDCWD, path, 0);
    let text = core::str::from_utf8(&buffer[..got.max(0) as usize]).unwrap_or("?");
    report.check("spawn2-open-action", status == 0 && text == "opened", format_args!("status={status} file={text:?}"));

    // Ninja's: its children's standard input is `/dev/null`.
    let null = SpawnAction {
        kind: spawn_action::OPEN,
        fd: 0,
        open_flags: o::RDONLY as u32,
        path: feos_user::path_at(feos_user::abi::at::FDCWD, "/dev/null"),
        ..SpawnAction::default()
    };
    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, &[PROGRAM, "--read-eof", "0"]);
    let actions = [null];
    request.actions = actions.as_ptr() as u64;
    request.action_count = 1;
    let pid = spawn2(&request);
    let status = if pid > 0 { wait(pid as usize) } else { pid };
    report.check("spawn2-open-dev-null", status == 0, format_args!("status={status}"));

    let chdir = SpawnAction { kind: spawn_action::CHDIR, path: feos_user::path_at(feos_user::abi::at::FDCWD, "/tmp"), ..SpawnAction::default() };
    let mut argv = [UserStr::default(); 8];
    let (_, status, out) = capture(child(&mut argv, &[PROGRAM, "--cwd"]), &[chdir]);
    let mut own = [0u8; 64];
    let length = feos_user::getcwd(&mut own);
    let own = core::str::from_utf8(&own[..length.max(0) as usize]).unwrap_or("?");
    report.check("spawn2-chdir-action", status == 0 && out.text() == "/tmp" && own != "/tmp", format_args!("child in {:?}, this in {own:?}", out.text()));
}

/// The child's pid, group, session and parent, as it reports them.
fn ids_of(out: &Captured) -> [isize; 4] {
    let mut ids = [-1isize; 4];
    for (slot, word) in ids.iter_mut().zip(out.text().split(' ')) {
        *slot = feos_user::parse_number(word).map_or(-1, |value| value as isize);
    }
    ids
}

/// Starts this program waiting for its standard input to end, which is the read end of
/// a pipe the caller keeps the write end of. The pid and the write end.
fn waiting_child(flags: u64, pgroup: i64) -> (isize, usize) {
    let Ok((read_end, write_end)) = pipe2(o::CLOEXEC) else {
        return (-1, 0);
    };
    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, &[PROGRAM, "--wait-stdin"]);
    let actions = [dup2(read_end, 0)];
    request.actions = actions.as_ptr() as u64;
    request.action_count = 1;
    request.flags = flags;
    request.pgroup = pgroup;
    let pid = spawn2(&request);
    close(read_end);
    (pid, write_end)
}

fn test_groups(report: &mut Report) {
    let me = getpid();
    let my_sid = getsid(0);
    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, &[PROGRAM, "--ids"]);
    request.flags = spawn::SETPGROUP;
    let (pid, status, out) = capture(request, &[]);
    let [child_pid, pgid, sid, ppid] = ids_of(&out);
    let own_group = status == 0 && child_pid == pid && pgid == pid && sid == my_sid;

    // A second child joins the first's group while it waits.
    let (leader, release) = waiting_child(spawn::SETPGROUP, 0);
    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, &[PROGRAM, "--ids"]);
    request.flags = spawn::SETPGROUP;
    request.pgroup = leader as i64;
    let (_, joined_status, joined) = capture(request, &[]);
    let joined_group = ids_of(&joined)[1];
    // And this one is put in its own group by its parent, after it's started.
    let (third, release_third) = waiting_child(0, 0);
    let before = getpgid(third as usize);
    let moved = setpgid(third as usize, third as usize);
    let after = getpgid(third as usize);
    close(release);
    close(release_third);
    let ended = (wait(leader as usize), wait(third as usize));
    report.check(
        "spawn2-setpgroup",
        own_group && joined_status == 0 && joined_group == leader && before == getpgid(0) && moved == 0 && after == third && ended == (0, 0),
        format_args!(
            "child {child_pid} pgid {pgid} sid {sid} (this {me} in {my_sid}); joined {leader}'s: {joined_group}; \
             setpgid {third}: {before}->{after} ({moved})"
        ),
    );
    report.check("getppid", ppid == me && getppid() == 1, format_args!("child's parent {ppid}, this one's {}", getppid()));

    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, &[PROGRAM, "--ids"]);
    request.flags = spawn::SETSID;
    let (pid, status, out) = capture(request, &[]);
    let [_, pgid, sid, _] = ids_of(&out);
    report.check("spawn2-setsid", status == 0 && pgid == pid && sid == pid && sid != my_sid, format_args!("child {pid}: pgid {pgid} sid {sid}"));

    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, &[PROGRAM, "--ids"]);
    request.flags = spawn::SETSIGMASK | spawn::SETSIGDEF | spawn::SETPGROUP | spawn::USEVFORK;
    request.sigmask = 1 << (10 - 1) | 1 << (9 - 1);
    request.sigdefault = 1 << (13 - 1);
    let (pid, status, out) = capture(request, &[]);
    let pgid = ids_of(&out)[1];
    report.check("spawn2-flags-combined", status == 0 && pgid == pid, format_args!("child {pid} in group {pgid}"));
}

fn test_synchronous_errors(report: &mut Report) {
    let mut argv = [UserStr::default(); 8];
    for (name, path, wanted) in [
        ("spawn2-enoent-synchronous", "/bin/no-such-program", errno::ENOENT),
        ("spawn2-eacces-synchronous", "/tmp", errno::EACCES),
        ("spawn2-enoexec-synchronous", "/etc/motd", errno::ENOEXEC),
    ] {
        let strings = user_strs(&[path], &mut argv);
        let result = spawn2(&spawn_request(path, strings, &[], &[]));
        report.check(name, result == -wanted, format_args!("{path}: {result}"));
        if result > 0 {
            wait(result as usize);
        }
    }
    // v1's `SPAWN` still gives a pid, whose child says why.
    let pid = feos_user::spawn("/bin/no-such-program", &["/bin/no-such-program"]);
    let status = if pid > 0 { wait(pid as usize) } else { pid };
    report.check("v1-spawn-errors-unchanged", pid > 0 && status == 127, format_args!("pid {pid}, status {status}"));
}

fn test_big_args(report: &mut Report) {
    // Seven 128 KiB strings and one a little shorter, with this program's name and mode:
    // within `ARG_MAX`, strings and pointers together, by a few bytes.
    const BIG: usize = spawn::STRING_MAX;
    let area = feos_user::mmap2(0, 8 * BIG, feos_user::abi::prot::READ | feos_user::abi::prot::WRITE, feos_user::abi::map::PRIVATE | feos_user::abi::map::ANONYMOUS);
    if area < 0 {
        return report.check("spawn2-1mib-args", false, format_args!("mmap2: {area}"));
    }
    let area = area as usize;
    // SAFETY: just mapped, writable and this long.
    unsafe { core::ptr::write_bytes(area as *mut u8, b'a', 8 * BIG) };
    let head = [PROGRAM, "--count"];
    let fixed: usize = head.iter().map(|word| word.len() + 1).sum::<usize>() + 10 * 8;
    let last = spawn::ARG_MAX - fixed - 7 * (BIG + 1) - 1;
    let mut argv = [UserStr::default(); 10];
    user_strs(&head, &mut argv);
    for (index, slot) in argv[2..].iter_mut().enumerate() {
        *slot = UserStr { pointer: area + index * BIG, length: if index == 7 { last } else { BIG } };
    }
    let (pid, status, out) = capture(spawn_request(PROGRAM, &argv, &[], &[]), &[]);
    let total = 7 * BIG + last;
    let mut text = [0u8; 24];
    let wanted_count = number(total, &mut text, b' ');
    let good = status == 0 && out.text().starts_with("10") && out.text().ends_with(wanted_count) && out.text().contains("all-a");
    // One byte more is too much.
    argv[9].length += 1;
    let over = spawn2(&spawn_request(PROGRAM, &argv, &[], &[]));
    feos_user::munmap(area, 8 * BIG);
    report.check(
        "spawn2-1mib-args",
        good && over == -errno::E2BIG,
        format_args!("pid {pid} status {status} child said {:?}; a byte more: {over}", out.text()),
    );
}

extern "C" {
    /// The ELF header, where the linker loaded it: the start of the read-only segment.
    static __ehdr_start: u8;
}

fn test_auxv(report: &mut Report) {
    let header = core::ptr::addr_of!(__ehdr_start) as usize;
    // SAFETY: the ELF header is loaded, read-only, at that address.
    let (phoff, phnum) = unsafe { (core::ptr::read_unaligned((header + 32) as *const u64), core::ptr::read_unaligned((header + 56) as *const u16)) };
    let phdr = aux_value(aux::PHDR);
    let random = aux_value(aux::RANDOM);
    let mut bytes = [0u8; 16];
    if random != 0 {
        // SAFETY: the kernel put 16 bytes there.
        unsafe { core::ptr::copy_nonoverlapping(random as *const u8, bytes.as_mut_ptr(), 16) };
    }
    let page = aux_value(aux::PAGESZ);
    let good = page == PAGE_SIZE as u64
        && phdr == header as u64 + phoff
        && aux_value(aux::PHNUM) == u64::from(phnum)
        && aux_value(aux::PHENT) == 56
        && bytes != [0; 16]
        && aux_value(aux::FEOS_ABI) == 2
        && c_arg(0) == Some(PROGRAM.as_bytes());
    report.check("auxv-present", good, format_args!("AT_PAGESZ={page} AT_PHDR={phdr:#x} (headers at {:#x}) AT_RANDOM={random:#x}", header as u64 + phoff));

    // Started by a relative path after a `chdir`, and named `-sh`.
    let chdir = SpawnAction { kind: spawn_action::CHDIR, path: feos_user::path_at(feos_user::abi::at::FDCWD, "/bin"), ..SpawnAction::default() };
    let mut argv = [UserStr::default(); 8];
    let request = spawn_request("ptest", user_strs(&["-sh", "--exe"], &mut argv), &[], &[]);
    let (pid, status, out) = capture(request, &[chdir]);
    let text = out.text();
    let (aux_exe, current) = text.split_once(' ').unwrap_or(("", ""));
    report.check(
        "auxv-feos-exe",
        status == 0 && !aux_exe.is_empty() && aux_exe == current && aux_exe.ends_with("/ptest"),
        format_args!("pid {pid}: AT_FEOS_EXE={aux_exe:?} CURRENT_EXE={current:?}"),
    );
}

fn aux_value(kind: u64) -> u64 {
    feos_user::auxv(kind).unwrap_or(0)
}

fn test_sysinfo(report: &mut Report) {
    let Ok(info) = feos_user::sysinfo() else {
        return report.check("sysinfo-cpus", false, format_args!("SYSINFO failed"));
    };
    let machine = core::str::from_utf8(&info.machine).unwrap_or("?").trim_end_matches('\0');
    let wanted = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    let cpu = feos_user::getcpu();
    // A shorter buffer takes the start of it, and nothing past.
    let mut short = [0xffu8; 12];
    let result = syscall(feos_user::abi::nr::SYSINFO, [short.as_mut_ptr() as usize, 8, 0, 0, 0, 0]);
    let short_ok = result == 0 && short[8..] == [0xff; 4] && short[..4] == 2u32.to_ne_bytes();
    report.check(
        "sysinfo-cpus",
        info.cpus_online >= 1
            && info.cpus_online == info.cpus_configured
            && (cpu as u32) < info.cpus_online
            && info.abi_version == 2
            && info.page_size == 4096
            && info.arg_max == spawn::ARG_MAX as u64
            && info.fd_max == 1024
            && info.total_pages > info.free_pages
            && machine == wanted
            && short_ok,
        format_args!(
            "{} CPUs online, {} pages ({} free), {machine}, on CPU {cpu}, short copy {short_ok}",
            info.cpus_online, info.total_pages, info.free_pages
        ),
    );
    let at = aux_value(aux::FEOS_FEATURES);
    report.check(
        "sysinfo-features",
        info.features == feos_user::abi::FEATURES && info.features & 0x1_ffff == 0x1_ffff && at == info.features,
        format_args!("features {:#x}, AT_FEOS_FEATURES {at:#x}", info.features),
    );
}

fn test_rusage(report: &mut Report) {
    // Some CPU time, and some memory touched.
    let mut sum = 0u64;
    let started = feos_user::uptime_ms();
    while feos_user::uptime_ms() - started < 60 {
        for index in 0..10_000u64 {
            sum = core::hint::black_box(sum.wrapping_add(index * index));
        }
    }
    let area = feos_user::mmap2(0, 64 * PAGE_SIZE, feos_user::abi::prot::READ | feos_user::abi::prot::WRITE, feos_user::abi::map::PRIVATE | feos_user::abi::map::ANONYMOUS);
    if area > 0 {
        // SAFETY: just mapped, writable.
        unsafe { core::ptr::write_bytes(area as *mut u8, 1, 64 * PAGE_SIZE) };
    }
    let own = feos_user::getrusage(rusage::SELF);
    let thread = feos_user::getrusage(rusage::THREAD);
    let children = feos_user::getrusage(rusage::CHILDREN);
    let bad = syscall(feos_user::abi::nr::GETRUSAGE, [7, 0, 0, 0, 0, 0]);
    if area > 0 {
        feos_user::munmap(area as usize, 64 * PAGE_SIZE);
    }
    let good = match (own, thread, children) {
        (Ok(own), Ok(thread), Ok(children)) => {
            let ns = |time: feos_user::abi::Timeval| time.sec * 1_000_000 + time.usec;
            ns(own.utime) > 0 && ns(thread.utime) > 0 && own.maxrss >= 256 && own.minflt >= 64 && children == feos_user::abi::Rusage::default()
        }
        _ => false,
    };
    report.check(
        "getrusage-self",
        good && bad == -errno::EINVAL,
        format_args!(
            "utime {:?} maxrss {} KiB minflt {} (thread {:?}), bad who {bad} [{sum}]",
            own.map(|usage| usage.utime),
            own.map_or(-1, |usage| usage.maxrss),
            own.map_or(-1, |usage| usage.minflt),
            thread.map(|usage| usage.utime),
        ),
    );
}

/// What each child mode does.
fn child_mode(args: &Args) -> Option<i32> {
    let mode = args.get(1)?;
    Some(match mode {
        "--echo" => {
            let mut line = feos_user::Line::new(1);
            use core::fmt::Write as _;
            let _ = line.write_str("argv=");
            let mut index = 0;
            while let Some(arg) = c_arg(index) {
                let _ = line.write_str(core::str::from_utf8(arg).unwrap_or("?"));
                let _ = line.write_str("|");
                index += 1;
            }
            let _ = line.write_str("\nenv=");
            for entry in environment() {
                let _ = line.write_str(core::str::from_utf8(entry).unwrap_or("?"));
                let _ = line.write_str("|");
            }
            let _ = line.write_str("\n");
            line.flush();
            0
        }
        "--env" => {
            let value = getenv("FOO").map_or("(unset)", |value| core::str::from_utf8(value).unwrap_or("?"));
            println!("{value}");
            0
        }
        "--write-fd" => {
            let fd = args.get(2).and_then(feos_user::parse_number).unwrap_or(99);
            let text = args.get(3).unwrap_or("");
            i32::from(write(fd, text.as_bytes()) != text.len() as isize)
        }
        "--fd-state" => {
            let mut wrong = 0;
            for index in 2..args.len() {
                let spec = args.get(index).unwrap_or("");
                let fd = feos_user::parse_number(spec.get(1..).unwrap_or("")).unwrap_or(9999);
                let open = feos_user::fcntl(fd, feos_user::abi::f::GETFD, 0) >= 0;
                if spec.starts_with('o') != open {
                    println!("ptest child: fd {fd} is {}", if open { "open" } else { "closed" });
                    wrong += 1;
                }
            }
            wrong
        }
        "--read-eof" => {
            let fd = args.get(2).and_then(feos_user::parse_number).unwrap_or(99);
            let mut buffer = [0u8; 8];
            i32::from(read(fd, &mut buffer) != 0)
        }
        "--wait-stdin" => {
            let mut buffer = [0u8; 8];
            while read(0, &mut buffer) > 0 {}
            0
        }
        "--cwd" => {
            let mut buffer = [0u8; 256];
            let length = feos_user::getcwd(&mut buffer);
            println!("{}", core::str::from_utf8(&buffer[..length.max(0) as usize]).unwrap_or("?"));
            0
        }
        "--ids" => {
            println!("{} {} {} {}", getpid(), getpgid(0), getsid(0), getppid());
            0
        }
        "--count" => {
            let mut total = 0;
            let mut all_a = true;
            for index in 2..args.len() {
                let arg = args.get(index).unwrap_or("");
                total += arg.len();
                all_a &= arg.bytes().all(|byte| byte == b'a');
            }
            println!("{} {} {}", args.len(), if all_a { "all-a" } else { "not-all-a" }, total);
            0
        }
        "--exe" => {
            let exe = feos_user::auxv_string(aux::FEOS_EXE).unwrap_or(b"");
            let mut buffer = [0u8; 256];
            let length = feos_user::current_exe(&mut buffer);
            println!("{} {}", core::str::from_utf8(exe).unwrap_or("?"), core::str::from_utf8(&buffer[..length.max(0) as usize]).unwrap_or("?"));
            0
        }
        _ => return None,
    })
}

fn main(args: Args) -> i32 {
    if let Some(status) = child_mode(&args) {
        return status;
    }
    let mut report = Report { failed: 0 };
    test_argv_envp(&mut report);
    test_env_from_shell(&mut report);
    test_dup2(&mut report);
    test_close_actions(&mut report);
    test_open_actions(&mut report);
    test_groups(&mut report);
    test_synchronous_errors(&mut report);
    test_big_args(&mut report);
    test_auxv(&mut report);
    test_sysinfo(&mut report);
    test_rusage(&mut report);
    if report.failed == 0 {
        println!("ptest: all ok");
        0
    } else {
        println!("ptest: {} FAILED", report.failed);
        1
    }
}

entry!(main);
