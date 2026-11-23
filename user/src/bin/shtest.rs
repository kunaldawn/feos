//! Runs `/bin/sh` through its checks (stage 1c.12): `-c` strings, `$?`, a pipeline of
//! programs, redirections, relative program paths after `cd`, script files, `#!` scripts
//! the kernel hands to it, `execvp`'s fallback to it, the environment its programs get,
//! `exec`, `trap`, background jobs, and that a program it starts has only descriptors 0, 1
//! and 2. Reports each check, and exits zero if all passed.
//!
//! Each check starts the shell with standard input `/dev/null` and its output and error
//! on pipes this reads. Ctrl-C on a terminal is the boot test's to check, which drives an
//! interactive `/bin/sh` on the serial console.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use feos_user::abi::{at, o, poll, spawn_action, PollFd, SpawnAction, UserStr};
use feos_user::{close, entry, pipe2, println, read, spawn2, spawn_request, Args};

#[global_allocator]
static HEAP: feos_user::heap::UserHeap = feos_user::heap::UserHeap::new(16 << 20);

entry!(main);

const SH: &str = "/bin/sh";

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, good: bool, detail: core::fmt::Arguments<'_>) {
        if good {
            println!("shtest {name}: ok ({detail})");
        } else {
            println!("shtest {name}: FAILED ({detail})");
            self.failed += 1;
        }
    }
}

/// What a program did: its pid, its `$?`, and what it wrote to each stream.
struct Ran {
    pid: isize,
    status: i32,
    out: String,
    err: String,
}

fn dup2(from: usize, to: usize) -> SpawnAction {
    SpawnAction { kind: spawn_action::DUP2, fd: from as i32, to: to as i32, ..SpawnAction::default() }
}

/// Runs `argv` with `env`, standard input `/dev/null`, and its output and error read here
/// to their ends.
fn run_with(path: &str, argv: &[&str], env: &[&str]) -> Ran {
    run_keeping(path, argv, env, &[])
}

/// [`run_with`], the program also given `extra`'s descriptors: (this one's, the child's).
fn run_keeping(path: &str, argv: &[&str], env: &[&str], extra: &[(usize, usize)]) -> Ran {
    let mut ran = Ran { pid: -1, status: -1, out: String::new(), err: String::new() };
    let (Ok((out_read, out_write)), Ok((err_read, err_write))) = (pipe2(o::CLOEXEC), pipe2(o::CLOEXEC)) else {
        return ran;
    };
    let null = "/dev/null";
    let mut actions = alloc::vec![
        SpawnAction { kind: spawn_action::OPEN, fd: 0, open_flags: o::RDONLY as u32, path: feos_user::path_at(at::FDCWD, null), ..SpawnAction::default() },
        dup2(out_write, 1),
        dup2(err_write, 2),
    ];
    actions.extend(extra.iter().map(|&(from, to)| dup2(from, to)));
    let above = extra.iter().map(|&(_, to)| to as i32 + 1).max().unwrap_or(3).max(3);
    actions.push(SpawnAction { kind: spawn_action::CLOSEFROM, fd: above, ..SpawnAction::default() });
    let words: Vec<UserStr> = argv.iter().map(|word| UserStr { pointer: word.as_ptr() as usize, length: word.len() }).collect();
    let envp: Vec<UserStr> = env.iter().map(|word| UserStr { pointer: word.as_ptr() as usize, length: word.len() }).collect();
    let request = spawn_request(path, &words, &envp, &actions);
    ran.pid = spawn2(&request);
    close(out_write);
    close(err_write);
    // Both streams at once, so neither fills while the other is read.
    let mut open = [true, true];
    let mut buffer = [0u8; 1024];
    let mut out = Vec::new();
    let mut err = Vec::new();
    while ran.pid > 0 && (open[0] || open[1]) {
        let mut fds = [
            PollFd { fd: if open[0] { out_read as i32 } else { -1 }, events: poll::IN, revents: 0 },
            PollFd { fd: if open[1] { err_read as i32 } else { -1 }, events: poll::IN, revents: 0 },
        ];
        if feos_user::poll(&mut fds, 60_000_000_000, None) <= 0 {
            break;
        }
        for (index, fd) in [out_read, err_read].into_iter().enumerate() {
            if !open[index] || fds[index].revents == 0 {
                continue;
            }
            let got = read(fd, &mut buffer);
            if got <= 0 {
                open[index] = false;
            } else if index == 0 {
                out.extend_from_slice(&buffer[..got as usize]);
            } else {
                err.extend_from_slice(&buffer[..got as usize]);
            }
        }
    }
    close(out_read);
    close(err_read);
    if ran.pid > 0 {
        ran.status = match feos_user::wait4(ran.pid, 0) {
            Ok((_, status)) => feos_coreutils_status(status),
            Err(error) => error as i32,
        };
    }
    ran.out = String::from_utf8_lossy(&out).into_owned();
    ran.err = String::from_utf8_lossy(&err).into_owned();
    ran
}

/// `$?` from a `WAIT4` status.
fn feos_coreutils_status(status: i32) -> i32 {
    if status & 0x7f == 0 {
        (status >> 8) & 0xff
    } else {
        128 + (status & 0x7f)
    }
}

const ENV: &[&str] = &["PATH=/bin", "HOME=/"];

/// `sh -c script`.
fn sh(script: &str) -> Ran {
    run_with(SH, &[SH, "-c", script], ENV)
}

/// Writes a file, mode `mode`.
fn put(path: &str, text: &str, mode: usize) -> bool {
    let fd = feos_user::openat(at::FDCWD, path, o::WRONLY | o::CREAT | o::TRUNC | o::CLOEXEC, mode);
    if fd < 0 {
        return false;
    }
    let wrote = feos_user::write(fd as usize, text.as_bytes());
    close(fd as usize);
    feos_user::fchmodat(at::FDCWD, path, mode, 0);
    wrote == text.len() as isize
}

fn shown(ran: &Ran) -> String {
    alloc::format!("status {} out {:?} err {:?}", ran.status, ran.out, ran.err)
}

fn test_basics(report: &mut Report) {
    let ran = sh("echo hello $((6 * 7))");
    report.check("sh-c-echo", ran.status == 0 && ran.out == "hello 42\n" && ran.err.is_empty(), format_args!("{}", shown(&ran)));

    let ran = sh("false; echo $?; true; echo $?; (exit 7); echo $?");
    let exits = sh("exit 9");
    report.check(
        "sh-status",
        ran.status == 0 && ran.out == "1\n0\n7\n" && exits.status == 9,
        format_args!("{}; exit 9 gave {}", shown(&ran), exits.status),
    );
}

fn test_pipeline(report: &mut Report) {
    // More than a pipe holds goes through, so both stages ran at once; by name through
    // `PATH` and by path.
    let ran = sh("ptest --emit | ptest --count-stdin");
    let long = sh("/bin/ptest --emit 5000 | /bin/ptest --count-stdin | /bin/cat");
    let failed = sh("/bin/ptest --emit 3 | false; echo $?; set -o pipefail; false | /bin/ptest --emit 1 >/dev/null; echo $?");
    report.check(
        "sh-pipeline",
        ran.out == "lines=2000 bytes=32000\n" && long.out == "lines=5000 bytes=80000\n" && failed.out == "1\n1\n",
        format_args!("{} | {} | {}", shown(&ran), shown(&long), shown(&failed)),
    );
}

fn test_redirections(report: &mut Report) {
    let quiet = sh("ptest --both 2>/dev/null");
    let to_file = sh("ptest --both >/tmp/shtest-out; echo file:; cat /tmp/shtest-out");
    let merged = sh("ptest --both 2>&1");
    let both = sh("ptest --both >/tmp/shtest-out 2>&1; cat /tmp/shtest-out");
    let append = sh("echo first >/tmp/shtest-out; ptest --both >>/tmp/shtest-out 2>/dev/null; cat /tmp/shtest-out");
    let swapped = sh("ptest --both 3>&1 1>&2 2>&3 3>&-");
    let captured = sh("x=$(ptest --both 2>&1); echo \"[$x]\"");
    let fed = sh("ptest --count-stdin <<EOF\none\ntwo\nEOF");
    let good = quiet.out == "to-stdout\n"
        && quiet.err.is_empty()
        && to_file.out == "file:\nto-stdout\n"
        && to_file.err == "to-stderr\n"
        && merged.out == "to-stdout\nto-stderr\n"
        && merged.err.is_empty()
        && both.out == "to-stdout\nto-stderr\n"
        && append.out == "first\nto-stdout\n"
        && swapped.out == "to-stderr\n"
        && swapped.err == "to-stdout\n"
        && captured.out == "[to-stdout\nto-stderr]\n"
        && fed.out == "lines=2 bytes=8\n";
    report.check(
        "sh-redirections",
        good,
        format_args!(
            "{} | {} | {} | {} | {} | {} | {} | {}",
            shown(&quiet),
            shown(&to_file),
            shown(&merged),
            shown(&both),
            shown(&append),
            shown(&swapped),
            shown(&captured),
            shown(&fed)
        ),
    );
}

fn test_paths(report: &mut Report) {
    let ran = sh("cd /bin && ./ptest --emit 1 && cd / && bin/ptest --emit 1 && cd /tmp && /bin/ptest --cwd && ../bin/ptest --cwd");
    report.check(
        "sh-cd-relative-program",
        ran.out == "emit-line-00000\nemit-line-00000\n/tmp\n/tmp\n",
        format_args!("{}", shown(&ran)),
    );

    let written = put("/tmp/shtest-script", "echo script-arg-$1-$#\nf() { echo in-function-$1; }\nf two\nexit 3\n", 0o644);
    let ran = run_with(SH, &[SH, "/tmp/shtest-script", "one"], ENV);
    report.check(
        "sh-script-file",
        written && ran.status == 3 && ran.out == "script-arg-one-1\nin-function-two\n",
        format_args!("{}", shown(&ran)),
    );

    // The kernel runs `#!`'s interpreter, this shell, with the script's path after it.
    let written = put("/tmp/shtest-shebang", "#!/bin/sh\necho shebang-$1 $0\n", 0o755);
    let ran = run_with("/tmp/shtest-shebang", &["/tmp/shtest-shebang", "two"], ENV);
    report.check(
        "sh-shebang-via-exec",
        written && ran.status == 0 && ran.out == "shebang-two /tmp/shtest-shebang\n",
        format_args!("{}", shown(&ran)),
    );

    // No `#!`: the kernel says `ENOEXEC`, and `execvp` runs it through `/bin/sh`.
    let written = put("/tmp/shtest-plain", "echo plain-$1\n", 0o755);
    let ran = run_with("/bin/ptest", &["/bin/ptest", "--execvp", "/tmp/shtest-plain", "three"], ENV);
    report.check(
        "sh-enoexec-fallback",
        written && ran.status == 0 && ran.out == "plain-three\n",
        format_args!("{}", shown(&ran)),
    );
}

fn test_environment(report: &mut Report) {
    let ran = sh("export FOO=exported; ptest --env; FOO=temporary ptest --env; ptest --env; unset FOO; ptest --env");
    let given = run_with(SH, &[SH, "-c", "ptest --env"], &["PATH=/bin", "FOO=from-parent"]);
    let unexported = sh("FOO=local; ptest --env");
    report.check(
        "sh-env-to-children",
        ran.out == "exported\ntemporary\nexported\n(unset)\n" && given.out == "from-parent\n" && unexported.out == "(unset)\n",
        format_args!("{} | {} | {}", shown(&ran), shown(&given), shown(&unexported)),
    );
}

fn test_exec(report: &mut Report) {
    // The program is the shell's process: the same pid, and nothing after it runs.
    let ran = sh("echo shell=$$; exec ptest --pid; echo not-reached");
    let pid = ran.pid;
    let wanted = alloc::format!("shell={pid}\npid={pid}\n");
    let redirected = sh("exec >/tmp/shtest-exec; echo into-file; exec >/dev/null; ptest --both 2>/dev/null; cat /tmp/shtest-exec >&2");
    let missing = sh("exec /bin/no-such-program; echo not-reached");
    report.check(
        "sh-exec-builtin",
        ran.out == wanted && redirected.err == "into-file\n" && redirected.out.is_empty() && missing.status == 127 && missing.out.is_empty(),
        format_args!("{} | {} | {}", shown(&ran), shown(&redirected), shown(&missing)),
    );
}

fn test_traps(report: &mut Report) {
    let term = sh("trap 'echo caught-term' TERM; kill -TERM $$; echo after-$?");
    let int = sh("trap 'echo caught-int' INT; kill -INT $$; echo after-int");
    let untrapped = sh("kill -TERM $$; echo not-reached");
    let cleared = sh("trap 'echo no' TERM; trap - TERM; kill -TERM $$; echo not-reached");
    report.check(
        "sh-trap-term",
        term.out == "caught-term\nafter-0\n"
            && int.out == "caught-int\nafter-int\n"
            && untrapped.status == 143
            && untrapped.out.is_empty()
            && cleared.status == 143,
        format_args!("{} | {} | {} | {}", shown(&term), shown(&int), shown(&untrapped), shown(&cleared)),
    );

    // Started with `SIGTERM` ignored (`nohup sh -c`): a trap can't catch it and `trap -`
    // can't reset it, and a program the shell starts (here another shell) has it ignored
    // too. `SIGINT`, which wasn't, is still the shell's to trap.
    feos_user::signal::set_disposition(feos_user::abi::sig::TERM, feos_user::abi::sa::IGN);
    let ignored = sh("trap 'echo caught' TERM; kill -TERM $$; echo after-trap; trap - TERM; kill -TERM $$; echo after-reset; /bin/sh -c 'kill -TERM $$; echo child-survived'; trap 'echo caught-int' INT; kill -INT $$");
    feos_user::signal::set_disposition(feos_user::abi::sig::TERM, feos_user::abi::sa::DFL);
    report.check(
        "sh-ignored-on-entry",
        ignored.out == "after-trap\nafter-reset\nchild-survived\ncaught-int\n" && ignored.status == 0,
        format_args!("{}", shown(&ignored)),
    );

    // A background job of processes: `$!`, `wait` and its status, `kill %1`.
    let ran = sh("ptest --emit 2 >/dev/null & wait $!; echo waited-$?; /bin/spin 1 0 & kill $!; wait $!; echo killed-$?");
    report.check(
        "sh-background-jobs",
        ran.out == "waited-0\nkilled-143\n",
        format_args!("{}", shown(&ran)),
    );
}

fn test_fds(report: &mut Report) {
    // Whatever the shell holds (its pipes, a here-document's, a capture's, a script),
    // its programs have 0, 1 and 2 and nothing else.
    let ran = sh("ptest --fds; ptest --fds | /bin/cat; ptest --fds <<EOF\nx\nEOF\necho \"$(ptest --fds)\"; exec 3</dev/null; ptest --fds; ptest --fds 4>/dev/null");
    report.check(
        "sh-no-fd-leak",
        ran.out == "fds=0,1,2\nfds=0,1,2\nfds=0,1,2\nfds=0,1,2\nfds=0,1,2,3\nfds=0,1,2,3,4\n",
        format_args!("{}", shown(&ran)),
    );
    // What the shell inherited open (a jobserver's pipe through `sh -c`, say), its
    // programs inherit, in the table (3) and past it (12), unless a redirection closes it.
    let fd = feos_user::openat(at::FDCWD, "/dev/null", o::RDONLY | o::CLOEXEC, 0);
    let ran = if fd < 0 {
        Ran { pid: -1, status: fd as i32, out: String::new(), err: String::new() }
    } else {
        let script = "ptest --fds; ptest --fds 3>&-; ptest --fds 3>&- | /bin/cat; /bin/sh -c 'ptest --fds'; exec ptest --fds";
        let ran = run_keeping(SH, &[SH, "-c", script], ENV, &[(fd as usize, 3), (fd as usize, 12)]);
        close(fd as usize);
        ran
    };
    report.check(
        "sh-inherited-fds",
        ran.out == "fds=0,1,2,3,12\nfds=0,1,2,12\nfds=0,1,2,12\nfds=0,1,2,3,12\nfds=0,1,2,3,12\n",
        format_args!("{}", shown(&ran)),
    );
}

fn main(_args: Args) -> i32 {
    let mut report = Report { failed: 0 };
    test_basics(&mut report);
    test_pipeline(&mut report);
    test_redirections(&mut report);
    test_paths(&mut report);
    test_environment(&mut report);
    test_exec(&mut report);
    test_traps(&mut report);
    test_fds(&mut report);
    for path in ["/tmp/shtest-out", "/tmp/shtest-script", "/tmp/shtest-shebang", "/tmp/shtest-plain", "/tmp/shtest-exec"] {
        feos_user::unlinkat(at::FDCWD, path, 0);
    }
    if report.failed == 0 {
        println!("shtest: all ok");
        0
    } else {
        println!("shtest: {} failed", report.failed);
        1
    }
}
