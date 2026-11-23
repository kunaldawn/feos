//! t3-process: std::process::Command through posix_spawn (relibc's over SPAWN2), with
//! pipes, exit statuses through waitpid (WAIT4), kill (KILL), and a child's stdin.
//!
//!     t3-process             the spawning checks, one pipe per child
//!     t3-process output      Command::output, stdout and stderr piped at once: std
//!                            reads both with poll(2) (POLL, kernel stage 1c.11)
//!     t3-process overflow    stack overflows in children (/bin/t3-overflow)
use std::io::{ErrorKind, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const HELLO: &str = "/bin/t3-hello";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("overflow") => return overflow(),
        Some("output") => return output(),
        _ => {}
    }
    // stdout piped, arguments, exit status.
    let (status, stdout) = stdout_of(Command::new(HELLO).args(["one", "two three"]));
    assert!(status.success(), "{status:?}");
    assert!(stdout.contains("hello from Rust std on feos"), "{stdout}");
    assert!(stdout.contains("arg[2]=two three"), "{stdout}");
    println!("stdout piped: {} bytes, {}", stdout.len(), status);

    let status = Command::new(HELLO).args(["--exit", "7"]).status().unwrap();
    assert_eq!(status.code(), Some(7));
    assert!(!status.success());
    println!("status: exit code {:?}", status.code());

    // stderr apart from stdout.
    let mut child = Command::new(HELLO).args(["--stderr", "to stderr"])
        .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(err, "to stderr\n");
    println!("stderr: piped apart");

    // The environment and the working directory (posix_spawn_file_actions_addchdir_np).
    let (_, out) = stdout_of(Command::new(HELLO).args(["--env", "T3_CHILD"]).env("T3_CHILD", "given"));
    assert_eq!(out, "T3_CHILD=given\n");
    let (_, out) = stdout_of(Command::new(HELLO).args(["--env", "T3_CHILD"]).env_clear());
    assert_eq!(out, "T3_CHILD=unset\n");
    let (_, out) = stdout_of(Command::new(HELLO).arg("--cwd").current_dir("/tmp"));
    assert_eq!(out, "cwd=/tmp\n");
    println!("env and current_dir: ok");

    // A child's stdin through a pipe.
    let mut child = Command::new("/bin/t3-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for i in 0..1000 {
            writeln!(stdin, "line {i}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("t3-stdin: 1000 lines, 8890 bytes"), "{text}");
    println!("stdin pipe: {}", text.trim());

    // try_wait, kill, the signal in the status.
    let mut child = Command::new(HELLO).args(["--sleep", "30"]).spawn().unwrap();
    assert!(child.try_wait().unwrap().is_none());
    let start = Instant::now();
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9), "{status:?}");
    assert!(start.elapsed() < Duration::from_secs(20));
    println!("kill: {status}");

    // A panicking child exits with 101 and says why on stderr.
    let (status, err) = stderr_of(Command::new(HELLO).arg("--panic"));
    assert_eq!(status.code(), Some(101));
    assert!(err.contains("t3-hello was asked to panic"), "{err}");
    println!("child panic: exit 101");

    // A missing program is the spawn's own error.
    let e = Command::new("/bin/t3-no-such-program").spawn().unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound, "{e:?}");
    // PATH search (posix_spawnp).
    let out = Command::new("t3-hello").arg("--exit").arg("0").status().unwrap();
    assert!(out.success());
    println!("not found, PATH: ok");

    // Reading a child's stdout as it runs.
    let mut child = Command::new(HELLO).arg("streamed").stdout(Stdio::piped()).spawn().unwrap();
    let mut s = String::new();
    child.stdout.take().unwrap().read_to_string(&mut s).unwrap();
    assert!(s.contains("arg[1]=streamed"));
    assert!(child.wait().unwrap().success());
    println!("t3-process: ok");
}

/// Runs `cmd` with its stdout piped (stderr inherited), returning its status and output.
fn stdout_of(cmd: &mut Command) -> (ExitStatus, String) {
    let mut child = cmd.stdout(Stdio::piped()).spawn().unwrap();
    let mut s = String::new();
    child.stdout.take().unwrap().read_to_string(&mut s).unwrap();
    (child.wait().unwrap(), s)
}

/// Runs `cmd` with its stderr piped (stdout to /dev/null).
fn stderr_of(cmd: &mut Command) -> (ExitStatus, String) {
    let mut child = cmd.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let mut s = String::new();
    child.stderr.take().unwrap().read_to_string(&mut s).unwrap();
    (child.wait().unwrap(), s)
}

fn output() {
    // Both pipes at once: std's read2 polls them.
    let out = Command::new(HELLO).args(["one", "two three"]).output().unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("arg[2]=two three"), "{stdout}");
    assert!(out.stderr.is_empty());
    let out = Command::new(HELLO).args(["--stderr", "to stderr"]).output().unwrap();
    assert_eq!(out.stdout, b"");
    assert_eq!(out.stderr, b"to stderr\n");
    println!("output: {} bytes of stdout, stderr apart", stdout.len());
    println!("t3-process output: ok");
}

fn overflow() {
    // Stack overflows are reported, then abort (SIGABRT), on the main thread and another.
    for (which, thread) in [("main", "main"), ("thread", "t3-overflow-thread")] {
        let (status, err) = stderr_of(Command::new("/bin/t3-overflow").arg(which));
        assert!(
            err.contains(&format!("thread '{thread}' (")) && err.contains(") has overflowed its stack"),
            "{which}: {status:?}, stderr {err:?}"
        );
        assert_eq!(status.signal(), Some(6), "{which}: {status:?}");
        println!("stack overflow ({which}): reported, {status}");
    }
    println!("t3-process overflow: ok");
}
