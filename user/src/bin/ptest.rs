//! Runs through starting programs (stages 1c.6 to 1c.8): `SPAWN2`'s arguments,
//! environment and actions, its synchronous errors, process groups and sessions, the
//! initial stack's aux vector, `SYSINFO` and `GETRUSAGE`; `EXEC` and `#!` scripts;
//! `WAIT4` and its statuses, `KILL` with default actions, stops and continues, and how
//! long a pid and a record last.
//! Reports each check, and exits zero if all passed.
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

/// Copies `from` to a new file `to`, executable: `false` if any of it fails.
fn copy_file(from: &str, to: &str) -> bool {
    let source = feos_user::openat(feos_user::abi::at::FDCWD, from, o::RDONLY | o::CLOEXEC, 0);
    let target = feos_user::openat(feos_user::abi::at::FDCWD, to, o::WRONLY | o::CREAT | o::TRUNC | o::CLOEXEC, 0o755);
    let mut good = source >= 0 && target >= 0;
    let mut buffer = [0u8; 4096];
    while good {
        let got = read(source as usize, &mut buffer);
        if got <= 0 {
            good = got == 0;
            break;
        }
        good = write(target as usize, &buffer[..got as usize]) == got;
    }
    for fd in [source, target] {
        if fd >= 0 {
            close(fd as usize);
        }
    }
    good
}

/// `SPAWN_EMPTY_PATH` runs the file the descriptor holds, whatever its path names now: a
/// file that isn't a program renamed over it, and then nothing there at all.
fn test_empty_path(report: &mut Report) {
    const COPY: &str = "/tmp/ptest-empty-path";
    const OTHER: &str = "/tmp/ptest-empty-path-other";
    let copied = copy_file(PROGRAM, COPY);
    // Not close-on-exec: it's looked up in the child's table.
    let fd = feos_user::openat(feos_user::abi::at::FDCWD, COPY, o::RDONLY, 0);
    let other = feos_user::openat(feos_user::abi::at::FDCWD, OTHER, o::WRONLY | o::CREAT | o::TRUNC | o::CLOEXEC, 0o755);
    if other >= 0 {
        write(other as usize, b"not a program\n");
        close(other as usize);
    }
    let replaced = feos_user::renameat(feos_user::abi::at::FDCWD, OTHER, feos_user::abi::at::FDCWD, COPY, 0);
    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, &[PROGRAM, "--echo", "held"]);
    request.path = feos_user::path_at(fd as i32, "");
    request.flags = spawn::EMPTY_PATH;
    let (_, renamed_status, renamed_out) = capture(request, &[]);
    let renamed_ran = renamed_out.text().starts_with("argv=/bin/ptest|--echo|held|");
    let unlinked = feos_user::unlink(COPY);
    let (_, unlinked_status, unlinked_out) = capture(request, &[]);
    let unlinked_ran = unlinked_out.text().starts_with("argv=/bin/ptest|--echo|held|");
    if fd >= 0 {
        close(fd as usize);
    }
    report.check(
        "spawn2-empty-path-identity",
        copied && fd >= 0 && replaced == 0 && unlinked == 0 && renamed_status == 0 && renamed_ran && unlinked_status == 0 && unlinked_ran,
        format_args!("fd {fd}, renamed over {replaced}: status {renamed_status}; unlinked {unlinked}: status {unlinked_status}"),
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
            // Children's use is `getrusage-children`'s: some have been reaped by now.
            ns(own.utime) > 0 && ns(thread.utime) > 0 && own.maxrss >= 256 && own.minflt >= 64 && ns(children.utime) >= 0
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

/// A copy of this program somewhere else, so a program it becomes has another path.
const EXEC_COPY: &str = "/tmp/ptest-exec";
/// A script `#!/bin/ptest --script`, and one whose interpreter is that script.
const SCRIPT: &str = "/tmp/ptest-script";
const SCRIPT2: &str = "/tmp/ptest-script2";

/// Makes the file `path` with `text` in it, executable.
fn make_executable(path: &str, text: &[u8]) -> bool {
    let fd = feos_user::openat(feos_user::abi::at::FDCWD, path, o::WRONLY | o::CREAT | o::TRUNC | o::CLOEXEC, 0o755);
    if fd < 0 {
        return false;
    }
    let good = write(fd as usize, text) == text.len() as isize;
    close(fd as usize);
    good && feos_user::fchmodat(feos_user::abi::at::FDCWD, path, 0o755, 0) == 0
}

/// Runs this program in `mode` and the `words` after it, its output captured.
fn capture_mode(words: &[&str]) -> (isize, isize, Captured) {
    let mut argv = [UserStr::default(); 8];
    capture(child(&mut argv, words), &[])
}

fn test_exec_replaces(report: &mut Report) {
    if !copy_file(PROGRAM, EXEC_COPY) {
        return report.check("exec-replaces", false, format_args!("no copy at {EXEC_COPY}"));
    }
    let (pid, status, out) = capture_mode(&[PROGRAM, "--exec-replaces"]);
    let mut text = [0u8; 24];
    let wanted_pid = number(pid.max(0) as usize, &mut text, b'=');
    let said = out.text();
    let wanted = [" pid", wanted_pid, " same=true exe=/tmp/ptest-exec proc=/tmp/ptest-exec aux=/tmp/ptest-exec"];
    let mut expected = [0u8; 128];
    let mut length = 0;
    for piece in wanted {
        expected[length..length + piece.len()].copy_from_slice(piece.as_bytes());
        length += piece.len();
    }
    let expected = core::str::from_utf8(&expected[..length]).unwrap_or("?");
    report.check(
        "exec-replaces",
        status == 0 && said.starts_with("after-exec") && said.ends_with(expected),
        format_args!("pid {pid} status {status}: {said:?}"),
    );
}

fn test_exec_fds(report: &mut Report) {
    let (_, closed, out) = capture_mode(&[PROGRAM, "--exec-fds", "cloexec"]);
    report.check("exec-closes-cloexec", closed == 0, format_args!("status {closed} {:?}", out.text()));
    let (_, kept, out) = capture_mode(&[PROGRAM, "--exec-fds", "kept"]);
    report.check("exec-keeps-fds", kept == 0, format_args!("status {kept} {:?}", out.text()));
}

fn test_exec_threads(report: &mut Report) {
    let (pid, status, out) = capture_mode(&[PROGRAM, "--exec-thread"]);
    report.check("exec-from-second-thread", status == 0, format_args!("pid {pid} status {status} {:?}", out.text()));
    let (pid, status, out) = capture_mode(&[PROGRAM, "--exec-blocked"]);
    report.check("exec-while-sibling-blocked", status == 0, format_args!("pid {pid} status {status} {:?}", out.text()));
}

fn test_exec_errors(report: &mut Report) {
    const NOT_A_PROGRAM: &str = "/tmp/ptest-not-a-program";
    const NOT_EXECUTABLE: &str = "/tmp/ptest-not-executable";
    let made = make_executable(NOT_A_PROGRAM, b"just words\n")
        && make_executable(NOT_EXECUTABLE, b"#!/bin/ptest --script\n")
        && feos_user::fchmodat(feos_user::abi::at::FDCWD, NOT_EXECUTABLE, 0o644, 0) == 0;
    let me = getpid();
    let missing = feos_user::exec_args("/bin/no-such-program", &["/bin/no-such-program"]);
    let text = feos_user::exec_args(NOT_A_PROGRAM, &[NOT_A_PROGRAM]);
    let directory = feos_user::exec_args("/tmp", &["/tmp"]);
    // Even root needs an execute bit on a file that keeps modes; a script makes sure the
    // refusal is about the mode, not what's in it.
    let no_x = feos_user::exec_args(NOT_EXECUTABLE, &[NOT_EXECUTABLE]);
    // `/etc/motd` has no POSIX mode to refuse it by, so it's judged as a program.
    let motd = feos_user::exec_args("/etc/motd", &["/etc/motd"]);
    let no_args = feos_user::exec(&feos_user::exec_request(PROGRAM, &[], &[]));
    for path in [NOT_A_PROGRAM, NOT_EXECUTABLE] {
        feos_user::unlinkat(feos_user::abi::at::FDCWD, path, 0);
    }
    // Still this program, as it was: its pid, its descriptors.
    let still = getpid() == me && write(1, b"") == 0;
    report.check(
        "exec-error-returns",
        made && missing == -errno::ENOENT
            && text == -errno::ENOEXEC
            && directory == -errno::EACCES
            && no_x == -errno::EACCES
            && motd == -errno::ENOEXEC
            && no_args == -errno::EINVAL
            && still,
        format_args!("missing {missing}, text {text}, directory {directory}, mode 644 {no_x}, /etc/motd {motd}, argc 0 {no_args}"),
    );
}

fn test_scripts(report: &mut Report) {
    let made = make_executable(SCRIPT, b"#!/bin/ptest --script\nnot read by anyone\n")
        && make_executable(SCRIPT2, b"#! /tmp/ptest-script  two words \t\n");
    let (_, status, out) = capture_mode(&[PROGRAM, "--exec", SCRIPT, "ignored", "one"]);
    let wanted = "script argv=/bin/ptest|--script|/tmp/ptest-script|one| exe=/tmp/ptest-script aux=/tmp/ptest-script";
    report.check("exec-script", made && status == 0 && out.text() == wanted, format_args!("status {status} {:?}", out.text()));
    let (_, status, out) = capture_mode(&[PROGRAM, "--exec", SCRIPT2, "ignored", "x"]);
    let wanted = "script argv=/bin/ptest|--script|/tmp/ptest-script|two words|/tmp/ptest-script2|x| exe=/tmp/ptest-script2 aux=/tmp/ptest-script2";
    report.check("exec-script-arg", made && status == 0 && out.text() == wanted, format_args!("status {status} {:?}", out.text()));
    // `SPAWN2` runs one the same way.
    let mut argv = [UserStr::default(); 8];
    let request = spawn_request(SCRIPT, user_strs(&[SCRIPT, "s"], &mut argv), &[], &[]);
    let (_, status, out) = capture(request, &[]);
    let wanted = "script argv=/bin/ptest|--script|/tmp/ptest-script|s| exe=/tmp/ptest-script aux=/tmp/ptest-script";
    report.check("spawn2-script", made && status == 0 && out.text() == wanted, format_args!("status {status} {:?}", out.text()));
}

/// A child that runs another program reading its standard input, which says it's ready
/// when it starts: its pid, and the write end of its input, which ends it when closed.
fn exec_ready_child(words: &[&str]) -> (isize, usize) {
    let (Ok((in_read, in_write)), Ok((out_read, out_write))) = (pipe2(o::CLOEXEC), pipe2(o::CLOEXEC)) else {
        return (-1, 0);
    };
    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, words);
    let actions = [dup2(in_read, 0), dup2(out_write, 1)];
    request.actions = actions.as_ptr() as u64;
    request.action_count = 2;
    let pid = spawn2(&request);
    close(in_read);
    close(out_write);
    let mut line = [0u8; 16];
    let got = if pid > 0 { read(out_read, &mut line) } else { pid };
    close(out_read);
    if got <= 0 || !line.starts_with(b"ready") {
        close(in_write);
        if pid > 0 {
            wait(pid as usize);
        }
        return (if pid > 0 { -2 } else { pid }, 0);
    }
    (pid, in_write)
}

fn test_exec_locks(report: &mut Report) {
    const KEPT: &str = "/tmp/ptest-lock-kept";
    const CLOSED: &str = "/tmp/ptest-lock-closed";
    let made = make_executable(KEPT, b"k") && make_executable(CLOSED, b"c");
    let (pid, release) = exec_ready_child(&[PROGRAM, "--exec-locks", CLOSED, KEPT]);
    // What the child's locks look like from another process: this one.
    let probe = |path: &str| {
        let fd = feos_user::openat(feos_user::abi::at::FDCWD, path, o::RDWR | o::CLOEXEC, 0);
        if fd < 0 {
            return (-1i16, 0i32);
        }
        let mut lock = feos_user::record(feos_user::abi::f::WRLCK, 0, 0);
        let result = feos_user::fcntl_lock(fd as usize, feos_user::abi::f::GETLK, &mut lock);
        close(fd as usize);
        if result != 0 {
            return (-2, result as i32);
        }
        (lock.kind, lock.pid)
    };
    let closed = probe(CLOSED);
    let kept = probe(KEPT);
    if release != 0 {
        close(release);
    }
    let status = if pid > 0 { wait(pid as usize) } else { pid };
    for path in [KEPT, CLOSED] {
        feos_user::unlinkat(feos_user::abi::at::FDCWD, path, 0);
    }
    report.check(
        "exec-drops-locks-of-cloexec-fds",
        made && pid > 0 && closed.0 == feos_user::abi::f::UNLCK && status == 0,
        format_args!("child {pid}: its lock on the close-on-exec file {closed:?}, status {status}"),
    );
    report.check(
        "exec-keeps-locks-of-kept-fds",
        made && pid > 0 && kept == (feos_user::abi::f::WRLCK, pid as i32),
        format_args!("child {pid}: its lock on the kept file {kept:?}"),
    );
}

/// A child that has run another program is out of its parent's hands for `SETPGID`.
fn test_setpgid_after_exec(report: &mut Report) {
    let (pid, release) = exec_ready_child(&[PROGRAM, "--exec", PROGRAM, PROGRAM, "--ready-wait"]);
    let moved = if pid > 0 { setpgid(pid as usize, pid as usize) } else { pid };
    if release != 0 {
        close(release);
    }
    let status = if pid > 0 { wait(pid as usize) } else { pid };
    report.check("setpgid-after-exec-eacces", moved == -errno::EACCES && status == 0, format_args!("child {pid}: setpgid {moved}, status {status}"));
}

/// The pipe `--exec-thread`'s first thread keeps writing to, and its process's pid.
static PIPE_WRITE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static PIPE_READ: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// `--exec-thread`'s second thread: runs this program again, telling it who it was.
fn exec_from_thread(_: usize) -> usize {
    use core::sync::atomic::Ordering;
    let (mut pid, mut tid, mut fd) = ([0u8; 24], [0u8; 24], [0u8; 24]);
    let pid = number(getpid() as usize, &mut pid, b'p');
    let tid = number(feos_user::gettid() as usize, &mut tid, b't');
    let fd = number(PIPE_READ.load(Ordering::Relaxed), &mut fd, b'f');
    let error = feos_user::exec_args(PROGRAM, &[PROGRAM, "--after-thread-exec", pid, tid, fd]);
    println!("exec from a thread: {error}");
    feos_user::exit(121)
}

/// `--exec-blocked`'s second thread: reads the pipe until it ends, which the exec makes it.
fn read_until_end(fd: usize) -> usize {
    let mut buffer = [0u8; 64];
    while read(fd, &mut buffer) > 0 {}
    0
}

/// A number after a one-byte prefix, as [`number`] made it.
fn after_prefix(arg: Option<&str>) -> usize {
    arg.and_then(|arg| arg.get(1..)).and_then(feos_user::parse_number).unwrap_or(usize::MAX)
}

/// The exec modes: each runs another program in place of this one, or is one run so.
fn exec_mode(mode: &str, args: &Args) -> Option<i32> {
    use core::sync::atomic::Ordering;
    Some(match mode {
        // `--exec <path> <argv...>`: runs `path` with exactly those arguments.
        "--exec" => {
            let mut words = [""; 8];
            let count = args.len().saturating_sub(3).min(words.len());
            for (index, word) in words[..count].iter_mut().enumerate() {
                *word = args.get(3 + index).unwrap_or("");
            }
            let path = args.get(2).unwrap_or("");
            let error = feos_user::exec_args(path, &words[..count]);
            println!("exec {path}: {error}");
            120
        }
        "--exec-replaces" => {
            let mut text = [0u8; 24];
            let pid = number(getpid() as usize, &mut text, b'p');
            let error = feos_user::exec_args(EXEC_COPY, &[EXEC_COPY, "--after-exec", pid]);
            println!("exec: {error}");
            120
        }
        "--after-exec" => {
            let mut exe = [0u8; 256];
            let length = feos_user::current_exe(&mut exe).max(0) as usize;
            let mut link = [0u8; 256];
            let linked = feos_user::readlinkat(feos_user::abi::at::FDCWD, "/proc/self/exe", &mut link).max(0) as usize;
            let aux = feos_user::auxv_string(aux::FEOS_EXE).unwrap_or(b"");
            fn text(bytes: &[u8]) -> &str {
                core::str::from_utf8(bytes).unwrap_or("?")
            }
            let same = after_prefix(args.get(2)) == getpid() as usize;
            println!("after-exec pid={} same={same} exe={} proc={} aux={}", getpid(), text(&exe[..length]), text(&link[..linked]), text(aux));
            0
        }
        "--exec-fds" => {
            let closing = open_file(true);
            let kept = open_file(false);
            let (mut first, mut second) = ([0u8; 24], [0u8; 24]);
            let spec = if args.get(2) == Some("cloexec") { number(closing, &mut first, b'c') } else { number(kept, &mut second, b'o') };
            let error = feos_user::exec_args(PROGRAM, &[PROGRAM, "--fd-state", spec]);
            println!("exec: {error}");
            120
        }
        "--exec-locks" => {
            let lock = |path: &str, cloexec: bool| {
                let flags = o::RDWR | if cloexec { o::CLOEXEC } else { 0 };
                let fd = feos_user::openat(feos_user::abi::at::FDCWD, path, flags, 0);
                let mut record = feos_user::record(feos_user::abi::f::WRLCK, 0, 0);
                fd >= 0 && feos_user::fcntl_lock(fd as usize, feos_user::abi::f::SETLK, &mut record) == 0
            };
            if !lock(args.get(2).unwrap_or(""), true) || !lock(args.get(3).unwrap_or(""), false) {
                return Some(119);
            }
            let error = feos_user::exec_args(PROGRAM, &[PROGRAM, "--ready-wait"]);
            println!("exec: {error}");
            120
        }
        "--ready-wait" => {
            println!("ready");
            let mut buffer = [0u8; 8];
            while read(0, &mut buffer) > 0 {}
            0
        }
        "--exec-thread" => {
            let Ok((read_end, write_end)) = pipe2(o::NONBLOCK) else {
                return Some(118);
            };
            PIPE_READ.store(read_end, Ordering::Relaxed);
            PIPE_WRITE.store(write_end, Ordering::Relaxed);
            if feos_user::thread::spawn(exec_from_thread, 0).is_err() {
                return Some(117);
            }
            // This, the first thread, writes until the exec ends it.
            let started = feos_user::uptime_ms();
            while feos_user::uptime_ms() - started < 10_000 {
                write(write_end, b"x");
                feos_user::sleep_ms(5);
            }
            116
        }
        "--after-thread-exec" => {
            let (pid, tid, fd) = (after_prefix(args.get(2)), after_prefix(args.get(3)), after_prefix(args.get(4)));
            let mut buffer = [0u8; 256];
            while read(fd, &mut buffer) > 0 {}
            feos_user::sleep_ms(100);
            let quiet = read(fd, &mut buffer) == -errno::EAGAIN;
            let (now_pid, now_tid) = (getpid() as usize, feos_user::gettid() as usize);
            println!("after-thread-exec pid {now_pid} (was {pid}) tid {now_tid} (was {tid}) quiet={quiet}");
            i32::from(!(now_pid == pid && now_tid == tid && tid != pid && quiet))
        }
        "--exec-blocked" => {
            let Ok((read_end, write_end)) = pipe2(o::CLOEXEC) else {
                return Some(118);
            };
            let mut argv = [UserStr::default(); 8];
            let mut request = child(&mut argv, &[PROGRAM, "--write-until-epipe"]);
            let actions = [dup2(write_end, 1)];
            request.actions = actions.as_ptr() as u64;
            request.action_count = 1;
            let writer = spawn2(&request);
            close(write_end);
            if writer <= 0 || feos_user::thread::spawn(read_until_end, read_end).is_err() {
                return Some(117);
            }
            // Long enough for the reader to be blocked in its read.
            feos_user::sleep_ms(50);
            let mut text = [0u8; 24];
            let writer = number(writer as usize, &mut text, b'w');
            let error = feos_user::exec_args(PROGRAM, &[PROGRAM, "--wait-pid", writer]);
            println!("exec: {error}");
            120
        }
        "--write-until-epipe" => {
            // It's `EPIPE` that's waited for, not `SIGPIPE`'s end (stage 1c.9).
            feos_user::signal::set_disposition(feos_user::abi::sig::PIPE, feos_user::abi::sa::IGN);
            let started = feos_user::uptime_ms();
            while feos_user::uptime_ms() - started < 10_000 {
                match write(1, b"x") {
                    result if result == -errno::EPIPE => return Some(0),
                    result if result < 0 => return Some(2),
                    _ => feos_user::sleep_ms(5),
                }
            }
            1
        }
        "--wait-pid" => {
            let status = wait(after_prefix(args.get(2)));
            println!("waited: {status}");
            status as i32
        }
        "--script" => {
            let mut line = feos_user::Line::new(1);
            use core::fmt::Write as _;
            let _ = line.write_str("script argv=");
            let mut index = 0;
            while let Some(arg) = c_arg(index) {
                let _ = line.write_str(core::str::from_utf8(arg).unwrap_or("?"));
                let _ = line.write_str("|");
                index += 1;
            }
            let mut exe = [0u8; 256];
            let length = feos_user::current_exe(&mut exe).max(0) as usize;
            let aux = feos_user::auxv_string(aux::FEOS_EXE).unwrap_or(b"");
            let _ = writeln!(line, " exe={} aux={}", core::str::from_utf8(&exe[..length]).unwrap_or("?"), core::str::from_utf8(aux).unwrap_or("?"));
            line.flush();
            0
        }
        _ => return None,
    })
}

/// What each child mode does.
fn child_mode(args: &Args) -> Option<i32> {
    let mode = args.get(1)?;
    if let Some(status) = exec_mode(mode, args) {
        return Some(status);
    }
    if let Some(status) = wait_mode(mode, args) {
        return Some(status);
    }
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
        // The whole environment as the kernel laid it out: every entry a `KEY=VALUE`, and
        // the aux vector after it intact to its end.
        "--env-whole" => {
            let value = getenv("FOO").map_or("(unset)", |value| core::str::from_utf8(value).unwrap_or("?"));
            let plain = environment().filter(|entry| !entry.contains(&b'=')).count();
            let exe = feos_user::auxv_string(aux::FEOS_EXE).unwrap_or(b"");
            let mut buffer = [0u8; 256];
            let length = feos_user::current_exe(&mut buffer);
            let whole = !exe.is_empty() && length > 0 && exe == &buffer[..length as usize];
            println!("env-whole FOO={value} plain={plain} exe={}", if whole { "ok" } else { "bad" });
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
    test_empty_path(&mut report);
    test_sysinfo(&mut report);
    test_rusage(&mut report);
    test_exec_replaces(&mut report);
    test_exec_fds(&mut report);
    test_exec_threads(&mut report);
    test_exec_errors(&mut report);
    test_scripts(&mut report);
    test_exec_locks(&mut report);
    test_setpgid_after_exec(&mut report);
    test_wait4_basics(&mut report);
    test_stops(&mut report);
    test_kill_ids(&mut report);
    test_stop_mid_call(&mut report);
    test_lifetimes(&mut report);
    for path in [EXEC_COPY, SCRIPT, SCRIPT2] {
        feos_user::unlinkat(feos_user::abi::at::FDCWD, path, 0);
    }
    if report.failed == 0 {
        println!("ptest: all ok");
        0
    } else {
        println!("ptest: {} FAILED", report.failed);
        1
    }
}

entry!(main);

// Stage 1c.8: `WAIT4`, exit records, `KILL` with default actions, stops.

/// A child of this program in `words`' mode, its standard output (if `out`) a pipe this
/// keeps the read end of, with `flags` and `pgroup` as `SPAWN2` takes them.
fn start_mode(words: &[&str], out: Option<usize>, flags: u64, pgroup: i64) -> isize {
    let mut argv = [UserStr::default(); 8];
    let mut request = child(&mut argv, words);
    let actions = out.map(|fd| [dup2(fd, 1)]);
    if let Some(actions) = &actions {
        request.actions = actions.as_ptr() as u64;
        request.action_count = 1;
    }
    request.flags = flags;
    request.pgroup = pgroup;
    spawn2(&request)
}

/// `wait4` of one child, blocking: its status, or the error as a status no child has.
fn status_of(pid: isize, options: usize) -> (isize, i32) {
    feos_user::wait4(pid, options).unwrap_or_else(|error| (error, -1))
}

fn test_wait4_basics(report: &mut Report) {
    use feos_user::abi::wait as w;
    let child = start_mode(&[PROGRAM, "--exit-code", "42"], None, 0, 0);
    let (pid, status) = status_of(child, 0);
    let seven = start_mode(&[PROGRAM, "--exit-code", "7"], None, 0, 0);
    let v1 = wait(seven as usize);
    report.check(
        "wait4-status-exit",
        pid == child && status == 0x2a00 && w::exited(status) && w::exit_code(status) == 42 && v1 == 7,
        format_args!("pid {pid} of {child} status {status:#x}, v1 wait {v1}"),
    );

    let (waiter, release) = waiting_child(0, 0);
    let early = feos_user::wait4(waiter, feos_user::abi::wait::NOHANG);
    close(release);
    let (pid, status) = status_of(waiter, 0);
    report.check(
        "wait4-wnohang",
        early == Ok((0, -1)) && pid == waiter && status == 0,
        format_args!("before the end {early:?}, after ({pid}, {status:#x})"),
    );

    // Ended by a signal: `SIGTERM` from here, and a fault's `SIGSEGV`, which v1 still
    // reports as 139.
    let (waiter, release) = waiting_child(0, 0);
    let sent = feos_user::kill(waiter, feos_user::abi::sig::TERM);
    let (pid, termed) = status_of(waiter, 0);
    close(release);
    report.check(
        "kill-term",
        sent == 0 && pid == waiter && w::signaled(termed) && w::term_signal(termed) == 15,
        format_args!("kill {sent}, status {termed:#x}"),
    );
    let faulter = start_mode(&[PROGRAM, "--fault"], None, 0, 0);
    let (_, faulted) = status_of(faulter, 0);
    let again = start_mode(&[PROGRAM, "--fault"], None, 0, 0);
    let v1 = wait(again as usize);
    report.check(
        "wait4-status-signal",
        termed == 15 && faulted == 11 && w::signaled(faulted) && v1 == 139,
        format_args!("SIGTERM {termed:#x}, fault {faulted:#x}, v1 of a fault {v1}"),
    );

    let burner = start_mode(&[PROGRAM, "--burn-ms", "80"], None, 0, 0);
    let used = feos_user::wait4_rusage(burner, 0);
    let good = used.is_ok_and(|(pid, status, usage)| {
        pid == burner && status == 0 && usage.utime.sec * 1_000_000 + usage.utime.usec >= 30_000 && usage.maxrss > 0 && usage.minflt > 0
    });
    report.check("wait4-rusage", good, format_args!("{used:?}"));

    // In children of their own, where nothing else of this program's can be reaped.
    let (_, _, out) = capture_mode(&[PROGRAM, "--wait-any"]);
    report.check("wait4-any", out.text() == "wait-any ok", format_args!("{:?}", out.text()));
    let (_, _, out) = capture_mode(&[PROGRAM, "--wait-pgroup"]);
    report.check("wait4-pgroup", out.text() == "wait-pgroup ok", format_args!("{:?}", out.text()));
    let (_, _, out) = capture_mode(&[PROGRAM, "--echild"]);
    report.check("wait4-echild", out.text() == "echild ok", format_args!("{:?}", out.text()));
}

/// What arrives on `fd`, a non-blocking read end, now: how many bytes.
fn drain(fd: usize) -> usize {
    let mut buffer = [0u8; 256];
    let mut total = 0;
    loop {
        let got = read(fd, &mut buffer);
        if got <= 0 {
            return total;
        }
        total += got as usize;
    }
}

fn test_stops(report: &mut Report) {
    use feos_user::abi::{sig, wait as w};
    let Ok((read_end, write_end)) = pipe2(o::CLOEXEC | o::NONBLOCK) else {
        return report.check("kill-stop-cont", false, format_args!("no pipe"));
    };
    let ticker = start_mode(&[PROGRAM, "--tick-pipe"], Some(write_end), 0, 0);
    close(write_end);
    feos_user::sleep_ms(50);
    let before = drain(read_end);
    let stop = feos_user::kill(ticker, sig::STOP);
    let stopped = status_of(ticker, w::UNTRACED);
    // Nothing more once it's reported stopped: every thread has parked.
    drain(read_end);
    feos_user::sleep_ms(100);
    let while_stopped = drain(read_end);
    let once = feos_user::wait4(ticker, w::UNTRACED | w::NOHANG);
    report.check(
        "wait4-wuntraced",
        stop == 0 && stopped == (ticker, 0x137f) && w::stopped(stopped.1) && w::stop_signal(stopped.1) == 19 && once == Ok((0, -1)),
        format_args!("kill {stop}, wait4 {stopped:?}, then {once:?}"),
    );
    let cont = feos_user::kill(ticker, sig::CONT);
    let continued = status_of(ticker, w::CONTINUED);
    feos_user::sleep_ms(50);
    let after = drain(read_end);
    report.check(
        "wait4-wcontinued",
        cont == 0 && continued == (ticker, 0xffff) && w::continued(continued.1),
        format_args!("kill {cont}, wait4 {continued:?}"),
    );
    let term = feos_user::kill(ticker, sig::TERM);
    let (_, ended) = status_of(ticker, 0);
    close(read_end);
    report.check(
        "kill-stop-cont",
        before > 0 && while_stopped == 0 && after > 0 && term == 0 && ended == 15,
        format_args!("ticks {before} running, {while_stopped} stopped, {after} continued; ended {ended:#x}"),
    );
}

/// The thread [`test_kill_ids`] starts: its id, then it waits to be let go.
static TID: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static LET_GO: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn wait_to_be_let_go(_: usize) -> usize {
    TID.store(feos_user::gettid() as usize, core::sync::atomic::Ordering::Release);
    while !LET_GO.load(core::sync::atomic::Ordering::Acquire) {
        feos_user::sleep_ms(5);
    }
    0
}

fn test_kill_ids(report: &mut Report) {
    use feos_user::abi::{sig, wait as w};
    let me = getpid();
    let (waiter, release) = waiting_child(0, 0);
    let own = feos_user::kill(me, 0);
    let other = feos_user::kill(waiter, 0);
    let alive = feos_user::wait4(waiter, w::NOHANG);
    let nobody = feos_user::kill(0x7fff_0000, 0);
    let shell = feos_user::kill(1, 0);
    let past = feos_user::kill(waiter, 65);
    let group = feos_user::kill(0, 0);
    close(release);
    let (_, status) = status_of(waiter, 0);
    report.check(
        "kill-zero-checks",
        own == 0 && other == 0 && alive == Ok((0, -1)) && nobody == -errno::ESRCH && shell == -errno::EPERM && past == -errno::EINVAL && group == 0 && status == 0,
        format_args!("self {own}, child {other} ({alive:?}), none {nobody}, shell {shell}, signal 65 {past}, group {group}"),
    );

    // A group of two, ended together.
    let (first, release_first) = waiting_child(spawn::SETPGROUP, 0);
    let (second, release_second) = waiting_child(spawn::SETPGROUP, first as i64);
    let sent = feos_user::kill(-first, sig::TERM);
    let (_, one) = status_of(first, 0);
    let (_, two) = status_of(second, 0);
    let gone = feos_user::kill(-first, 0);
    close(release_first);
    close(release_second);
    report.check(
        "kill-pgroup",
        sent == 0 && one == 15 && two == 15 && gone == -errno::ESRCH,
        format_args!("kill -{first} {sent}: {one:#x} {two:#x}, then {gone}"),
    );

    // One that has ended and isn't reaped is still there, and a signal to it does nothing.
    let quick = start_mode(&[PROGRAM, "--exit-code", "0"], None, 0, 0);
    feos_user::sleep_ms(200);
    let term = feos_user::kill(quick, sig::TERM);
    let zero = feos_user::kill(quick, 0);
    let (pid, status) = status_of(quick, 0);
    let after = feos_user::kill(quick, 0);
    report.check(
        "kill-unreaped-child-ok",
        term == 0 && zero == 0 && pid == quick && status == 0 && after == -errno::ESRCH,
        format_args!("SIGTERM {term}, 0 {zero}, reaped ({pid}, {status:#x}), then {after}"),
    );

    LET_GO.store(false, core::sync::atomic::Ordering::Release);
    let thread = feos_user::thread::spawn(wait_to_be_let_go, 0);
    while thread.is_ok() && TID.load(core::sync::atomic::Ordering::Acquire) == 0 {
        feos_user::sleep_ms(1);
    }
    let tid = TID.load(core::sync::atomic::Ordering::Acquire) as isize;
    let zero = feos_user::kill(tid, 0);
    let term = feos_user::kill(tid, sig::TERM);
    LET_GO.store(true, core::sync::atomic::Ordering::Release);
    let joined = thread.map(feos_user::thread::Thread::join);
    report.check(
        "kill-tid-esrch",
        tid > me && zero == -errno::ESRCH && term == -errno::ESRCH && joined == Ok(0),
        format_args!("tid {tid} of {me}: {zero} {term}, joined {joined:?}"),
    );
}

/// The file a child is stopped writing, and the one another writes meanwhile.
const BIG: &str = "/tmp/ptest-big";
const OTHER: &str = "/tmp/ptest-other";

fn test_stop_mid_call(report: &mut Report) {
    use feos_user::abi::{sig, wait as w};
    // The child says "go", then a dot for each MiB it has written.
    let Ok((read_end, write_end)) = pipe2(o::CLOEXEC | o::NONBLOCK) else {
        return report.check("stop-mid-call-no-lockup", false, format_args!("no pipe"));
    };
    let writer = start_mode(&[PROGRAM, "--big-write", BIG], Some(write_end), 0, 0);
    close(write_end);
    let mut said = 0;
    for _ in 0..2000 {
        said += drain(read_end);
        if said >= 2 {
            break;
        }
        feos_user::sleep_ms(1);
    }
    feos_user::sleep_ms(20);
    let stop = feos_user::kill(writer, sig::STOP);
    let stopped = status_of(writer, w::UNTRACED);
    // No more MiB while it's stopped, and another program writes the same mount meanwhile.
    let before = said + drain(read_end);
    let began = feos_user::uptime_ms();
    let other = start_mode(&[PROGRAM, "--write-mib", OTHER, "4"], None, 0, 0);
    let (_, other_status) = status_of(other, 0);
    let took = feos_user::uptime_ms() - began;
    feos_user::sleep_ms(50);
    let meanwhile = drain(read_end);
    let cont = feos_user::kill(writer, sig::CONT);
    let (_, ended) = status_of(writer, 0);
    let total = before + meanwhile + drain(read_end);
    let size = feos_user::fstatat(feos_user::abi::at::FDCWD, BIG, 0).map_or(-1, |stat| stat.size);
    close(read_end);
    feos_user::unlinkat(feos_user::abi::at::FDCWD, BIG, 0);
    feos_user::unlinkat(feos_user::abi::at::FDCWD, OTHER, 0);
    report.check(
        "stop-mid-call-no-lockup",
        stop == 0 && stopped == (writer, 0x137f) && meanwhile == 0 && other_status == 0 && cont == 0 && ended == 0 && total == 2 + 64 && size == 4 << 20,
        format_args!("stopped after {} MiB, {meanwhile} more while stopped; another wrote 4 MiB in {took} ms ({other_status}); ended {ended:#x} after {} MiB", before.saturating_sub(2), total.saturating_sub(2)),
    );
}

fn test_lifetimes(report: &mut Report) {
    // One child left unreaped while 2000 others come and go: none of them is given its pid.
    // Until the pid counter wraps none could be, so this only guards against pids going
    // round early; `ktest records` checks what holds one once the counter has wrapped.
    let zombie = start_mode(&[PROGRAM, "--exit-code", "0"], None, 0, 0);
    let mut reused = 0;
    let mut failed = 0;
    let mut last = 0;
    for _ in 0..2000 {
        let pid = feos_user::spawn("/bin/hello", &["/bin/hello", "--quiet"]);
        if pid <= 0 {
            failed += 1;
            continue;
        }
        last = pid;
        reused += usize::from(pid == zombie);
        if status_of(pid, 0) != (pid, 0) {
            failed += 1;
        }
    }
    let (pid, status) = status_of(zombie, 0);
    report.check(
        "pid-not-reused-while-unreaped",
        zombie > 0 && reused == 0 && failed == 0 && pid == zombie && status == 0,
        format_args!("zombie {zombie}, 2000 others to {last}, reused {reused}, failed {failed}"),
    );

    // A child whose parent has ended carries on, with no parent but the shell.
    let Ok((read_end, write_end)) = pipe2(o::CLOEXEC) else {
        return report.check("orphan-runs-on", false, format_args!("no pipe"));
    };
    let middle = start_mode(&[PROGRAM, "--orphan-middle"], Some(write_end), 0, 0);
    close(write_end);
    let (_, middle_status) = status_of(middle, 0);
    let mut captured = Captured { bytes: [0; 512], length: 0 };
    while captured.length < captured.bytes.len() {
        let got = read(read_end, &mut captured.bytes[captured.length..]);
        if got <= 0 {
            break;
        }
        captured.length += got as usize;
    }
    close(read_end);
    report.check(
        "orphan-runs-on",
        middle_status == 0 && captured.text() == "orphan ppid=1",
        format_args!("middle {middle_status:#x}, leaf said {:?}", captured.text()),
    );

    // What reaped children used, and theirs.
    let micros = |time: feos_user::abi::Timeval| time.sec * 1_000_000 + time.usec;
    let before = feos_user::getrusage(rusage::CHILDREN).map_or(-1, |usage| micros(usage.utime));
    let parent = start_mode(&[PROGRAM, "--burn-child"], None, 0, 0);
    let own = feos_user::wait4_rusage(parent, 0).map_or(-1, |(_, _, usage)| micros(usage.utime));
    let after = feos_user::getrusage(rusage::CHILDREN).map_or(-1, |usage| micros(usage.utime));
    report.check(
        "getrusage-children",
        before >= 0 && own >= 80_000 && after - before >= 80_000,
        format_args!("children {before} us, then {after} us; the child and its own {own} us"),
    );
}

/// Burns `ms` of CPU time, as nearly as the clock tells.
fn burn(ms: u64) {
    let mut sum = 0u64;
    let started = feos_user::uptime_ms() as u64;
    while (feos_user::uptime_ms() as u64) - started < ms {
        for index in 0..1000u64 {
            sum = core::hint::black_box(sum.wrapping_add(index * index));
        }
    }
    core::hint::black_box(sum);
}

/// The child modes of stage 1c.8's checks.
fn wait_mode(mode: &str, args: &Args) -> Option<i32> {
    use feos_user::abi::wait as w;
    Some(match mode {
        "--exit-code" => args.get(2).and_then(feos_user::parse_number).unwrap_or(99) as i32,
        "--fault" => {
            // SAFETY: not at all: the null page is never mapped, so this faults.
            unsafe { core::ptr::read_volatile(core::ptr::null::<u64>()) as i32 }
        }
        "--burn-ms" => {
            burn(args.get(2).and_then(feos_user::parse_number).unwrap_or(0) as u64);
            0
        }
        // Burns some itself, and has a child burn some, which it reaps.
        "--burn-child" => {
            burn(60);
            let grandchild = start_mode(&[PROGRAM, "--burn-ms", "60"], None, 0, 0);
            i32::from(status_of(grandchild, 0) != (grandchild, 0))
        }
        "--tick-pipe" => loop {
            if write(1, b".") < 0 {
                return Some(1);
            }
            feos_user::sleep_ms(2);
        },
        "--big-write" => {
            let path = args.get(2).unwrap_or(BIG);
            let fd = feos_user::openat(feos_user::abi::at::FDCWD, path, o::WRONLY | o::CREAT | o::TRUNC | o::CLOEXEC, 0o644);
            let chunk = feos_user::mmap2(0, 1 << 20, feos_user::abi::prot::READ | feos_user::abi::prot::WRITE, feos_user::abi::map::PRIVATE | feos_user::abi::map::ANONYMOUS);
            if fd < 0 || chunk < 0 {
                return Some(2);
            }
            // SAFETY: just mapped, writable.
            unsafe { core::ptr::write_bytes(chunk as *mut u8, b'x', 1 << 20) };
            write(1, b"go");
            // 64 MiB written, over the first 4 MiB of the file: the RAM filesystem `/tmp`
            // is on hasn't room for all of it at once.
            for index in 0..64 {
                // SAFETY: as above.
                let bytes = unsafe { core::slice::from_raw_parts(chunk as *const u8, 1 << 20) };
                if feos_user::pwrite(fd as usize, bytes, (index % 4) << 20) != 1 << 20 {
                    return Some(3);
                }
                write(1, b".");
            }
            close(fd as usize);
            0
        }
        "--write-mib" => {
            let path = args.get(2).unwrap_or(OTHER);
            let count = args.get(3).and_then(feos_user::parse_number).unwrap_or(1);
            let fd = feos_user::openat(feos_user::abi::at::FDCWD, path, o::WRONLY | o::CREAT | o::TRUNC | o::CLOEXEC, 0o644);
            if fd < 0 {
                return Some(2);
            }
            let block = [b'y'; 4096];
            for _ in 0..count * 256 {
                if write(fd as usize, &block) != block.len() as isize {
                    return Some(3);
                }
            }
            close(fd as usize);
            0
        }
        "--orphan-middle" => {
            let leaf = start_mode(&[PROGRAM, "--orphan-leaf"], None, 0, 0);
            i32::from(leaf <= 0)
        }
        "--orphan-leaf" => {
            for _ in 0..400 {
                if getppid() == 1 {
                    break;
                }
                feos_user::sleep_ms(5);
            }
            // A while more, so it's seen to carry on.
            feos_user::sleep_ms(50);
            println!("orphan ppid={}", getppid());
            0
        }
        "--wait-any" => {
            let a = start_mode(&[PROGRAM, "--exit-code", "3"], None, 0, 0);
            let b = start_mode(&[PROGRAM, "--exit-code", "4"], None, 0, 0);
            let first = status_of(-1, 0);
            let second = status_of(-1, 0);
            let none = feos_user::wait4(-1, 0);
            let good = (first == (a, 0x300) && second == (b, 0x400)) || (first == (b, 0x400) && second == (a, 0x300));
            if good && none == Err(-errno::ECHILD) {
                println!("wait-any ok");
            } else {
                println!("wait-any {a} {b}: {first:?} {second:?} then {none:?}");
            }
            0
        }
        "--wait-pgroup" => {
            let other = start_mode(&[PROGRAM, "--exit-code", "5"], None, spawn::SETPGROUP, 0);
            let mine = start_mode(&[PROGRAM, "--exit-code", "6"], None, 0, 0);
            // Neither waits for the other: each has ended by the time the other's is asked.
            feos_user::sleep_ms(100);
            let theirs = status_of(-other, 0);
            let own = status_of(0, 0);
            let none = feos_user::wait4(-other, w::NOHANG);
            if theirs == (other, 0x500) && own == (mine, 0x600) && none == Err(-errno::ECHILD) {
                println!("wait-pgroup ok");
            } else {
                println!("wait-pgroup {other} {mine}: {theirs:?} {own:?} then {none:?}");
            }
            0
        }
        "--echild" => {
            let any = feos_user::wait4(-1, 0);
            let own = feos_user::wait4(getpid(), 0);
            let parent = feos_user::wait4(getppid(), w::NOHANG);
            let group = feos_user::wait4(0, 0);
            let echild = Err(-errno::ECHILD);
            if any == echild && own == echild && parent == echild && group == echild {
                println!("echild ok");
            } else {
                println!("echild {any:?} {own:?} {parent:?} {group:?}");
            }
            0
        }
        // For the kernel's `ktest records`: a child whose thread the kernel refuses, so
        // the process whose thread it was has a record. Nothing else is started after, so
        // the slot the thread was refused is still free when the kernel looks at it.
        "--refused-thread-parent" => {
            let child = start_mode(&[PROGRAM, "--refused-thread"], None, 0, 0);
            let (pid, status) = status_of(child, 0);
            println!("refused-thread parent: child {child} ended {pid} {status:#x}");
            i32::from(pid != child || status != 0)
        }
        "--refused-thread" => match feos_user::thread::spawn(|_| 0, 0) {
            Err(error) if error == -errno::EAGAIN => 0,
            Ok(_) => 1,
            Err(_) => 2,
        },
        // For `ktest records`: an unreaped child, and a group whose leader is gone and
        // reaped but which has a member left. Says both pids, reaps the child once its
        // standard input has a byte, then waits for the input's end. The member reads a
        // pipe of its own, not that input, which ends as this does.
        "--hold-pids" => {
            let zombie = start_mode(&[PROGRAM, "--exit-code", "0"], None, 0, 0);
            let Ok((member_in, hold_open)) = pipe2(o::CLOEXEC) else {
                return Some(3);
            };
            let mut argv = [UserStr::default(); 8];
            let mut request = child(&mut argv, &[PROGRAM, "--group-leader"]);
            let actions = [dup2(member_in, 0)];
            request.actions = actions.as_ptr() as u64;
            request.action_count = 1;
            request.flags = spawn::SETPGROUP;
            let leader = spawn2(&request);
            close(member_in);
            let led = status_of(leader, 0) == (leader, 0);
            println!("hold {zombie} {leader}");
            let mut byte = [0u8; 1];
            read(0, &mut byte);
            let reaped = status_of(zombie, 0) == (zombie, 0);
            println!("reaped {reaped}");
            while read(0, &mut byte) > 0 {}
            close(hold_open);
            i32::from(!led || !reaped)
        }
        "--group-leader" => {
            let member = start_mode(&[PROGRAM, "--wait-stdin"], None, 0, 0);
            i32::from(member <= 0)
        }
        _ => return None,
    })
}
