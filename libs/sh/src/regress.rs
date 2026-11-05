//! Regression tests: the audit's reproductions, then one test per language
//! feature and the examples from `docs/terminal.md`.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use feos_coreutils::{BytesInput, BytesOutput, Input, NoSystem, Output, System, Vfs};

use crate::tests_support::NoHost;
use crate::{CancelToken, Host, Io, Shell};

/// Runs lines in one shell, returning the last status, stdout and stderr.
fn r(lines: &[&str]) -> (i32, String, String) {
    let mut shell = Shell::new(None, &NoSystem);
    run_in(&mut shell, lines, b"")
}

fn run_in(shell: &mut Shell<'_>, lines: &[&str], stdin: &[u8]) -> (i32, String, String) {
    let mut host = NoHost;
    let mut input = BytesInput::new(stdin.to_vec());
    let mut out = BytesOutput::new();
    let mut err = BytesOutput::new();
    let mut status = 0;
    {
        let mut io = Io {
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
        };
        for line in lines {
            status = shell.execute_line(line, &mut io, &mut host);
        }
    }
    (
        status,
        String::from_utf8_lossy(out.bytes()).into_owned(),
        String::from_utf8_lossy(err.bytes()).into_owned(),
    )
}

fn out(line: &str) -> String {
    r(&[line]).1
}

fn status(line: &str) -> i32 {
    r(&[line]).0
}

// ----- SH-1 to SH-5: input must not take the session down -----

#[test]
fn division_overflow_does_not_panic() {
    assert_eq!(
        out("echo $(( (-9223372036854775807 - 1) / -1 ))"),
        "-9223372036854775808\n"
    );
    assert_eq!(out("echo $(( (-9223372036854775807 - 1) % -1 ))"), "0\n");
    assert_eq!(status("expr -9223372036854775808 / -1"), 0);
    assert_eq!(status("expr 5 % 0"), 2);
}

#[test]
fn grep_word_match_survives_multibyte_text() {
    assert_eq!(
        out("printf '\\303\\251 x\\n' | grep -w x"),
        "\u{e9} x\n"
    );
    assert_eq!(status("printf '\\303\\251x\\n' | grep -qw x"), 0);
}

#[test]
fn nl_clamps_huge_widths() {
    let (_, output, _) = r(&["echo a > /f", "nl -w 70000 /f"]);
    assert!(output.ends_with("1\ta\n"));
    assert!(output.len() < 2000);
}

#[test]
fn printf_refuses_absurd_widths() {
    let (code, output, err) = r(&["printf '%99999999d' 1 | wc -c"]);
    assert_eq!(code, 0);
    assert_eq!(output.trim(), "0");
    assert!(err.contains("width"), "{err}");
    assert_eq!(status("printf '%.99999999d' 1"), 1);
    assert_eq!(status("printf '%*d' 99999999 1"), 1);
}

#[test]
fn doubling_a_variable_hits_the_budget() {
    let (code, _, err) = r(&["x=a; i=0; while [ $i -lt 40 ] && x=$x$x; do i=$((i+1)); done"]);
    assert_ne!(code, 130);
    assert!(err.contains("variable space") || err.contains("too large"), "{err}");
}

#[test]
fn oversized_pipelines_report_an_error() {
    let (code, _, err) = r(&["cat /dev/zero | wc -c"]);
    assert_eq!(code, 1);
    assert!(err.contains("too large"), "{err}");
}

#[test]
fn a_million_line_sort_stays_within_bounds() {
    let (code, output, _) = r(&["seq 1000000 | sort | wc -l"]);
    assert_eq!(code, 0);
    assert_eq!(output.trim(), "1000000");
}

#[test]
fn the_overlay_quota_reports_no_space() {
    let mut shell = Shell::new(None, &NoSystem);
    shell.fs.set_quota(16);
    let (code, _, err) = run_in(&mut shell, &["echo 0123456789012345678901234 > /f"], b"");
    assert_ne!(code, 0);
    assert!(err.contains("No space"), "{err}");
}

/// Runs a closure on a thread as small as a kernel stack, so that a recursion
/// escape kills the test instead of passing unnoticed.
fn on_small_stack<F: FnOnce() + Send + 'static>(body: F) {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(body)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn eval_recursion_is_an_error() {
    on_small_stack(|| {
        let (_, _, err) = r(&["x='eval \"$x\"'; eval \"$x\""]);
        assert!(err.contains("nesting"), "{err}");
    });
}

#[test]
fn self_sourcing_is_an_error() {
    on_small_stack(|| {
        let (_, output, err) = r(&["echo '. /a' > /a; . /a; echo survived"]);
        assert!(err.contains("nesting"), "{err}");
        // The nesting failure ends the line it happened on.
        assert_eq!(output, "");
        let mut shell = Shell::new(None, &NoSystem);
        let (_, output, _) = run_in(&mut shell, &["echo '. /a' > /a; . /a", "echo next"], b"");
        assert_eq!(output, "next\n");
    });
}

#[test]
fn function_recursion_is_an_error() {
    on_small_stack(|| {
        let (_, _, err) = r(&["f() { f; f; }; f"]);
        assert!(err.contains("nesting"), "{err}");
    });
}

#[test]
fn substitution_recursion_is_an_error() {
    on_small_stack(|| {
        let (_, _, err) = r(&["f() { echo $(f); }; f"]);
        assert!(err.contains("nesting"), "{err}");
        let (_, _, err) = r(&["echo 'sh /s' > /s; sh /s"]);
        assert!(err.contains("nesting"), "{err}");
    });
}

/// Finds how many levels of `line`'s recursion fit before the limit.
fn deepest(setup: &str) -> i64 {
    let (_, output, _) = r(&[setup]);
    output.lines().count() as i64
}

#[test]
fn ordinary_recursion_has_room_on_a_kernel_stack() {
    on_small_stack(|| {
        let functions = deepest("f() { echo x; f; }; f");
        let substitutions = deepest("f() { echo x >&2; echo x; x=$(f); echo $x | head -n 0; }; f");
        std::eprintln!("levels: functions {functions}, substitutions {substitutions}");
        assert!(functions >= 10, "only {functions} function levels");
        assert_eq!(out("f() { [ $1 -gt 0 ] && f $(($1 - 1)) || echo done; }; f 8"), "done\n");
    });
}

#[test]
fn deep_syntax_is_an_error() {
    on_small_stack(|| {
        for text in [
            "(".repeat(5000),
            "{ ".repeat(5000),
            "if true; then ".repeat(5000),
            "while :; do ".repeat(5000),
            "! ".repeat(100_000) + "true",
            alloc::format!("echo $(({}", "(".repeat(5000)),
            alloc::format!("echo ${}", "{x:-$".repeat(5000)),
            alloc::format!("[ {} a", "( ".repeat(5000)),
            alloc::format!("[ {} a ]", "! ".repeat(5000)),
            alloc::format!("expr {} 1", "( ".repeat(5000)),
            alloc::format!("[[ {} a ]]", "( ".repeat(5000)),
            alloc::format!("echo a | grep -E '{}'", "(".repeat(5000)),
        ] {
            let mut shell = Shell::new(None, &NoSystem);
            run_in(&mut shell, &[&text], b"");
        }
    });
}

#[test]
fn deep_directory_trees_stop_with_an_error() {
    on_small_stack(|| {
        let (_, _, err) = r(&["i=0; while [ $i -lt 100 ]; do mkdir a; cd a; i=$((i+1)); done"]);
        assert!(err.contains("deeply"), "{err}");
        let mut shell = Shell::new(None, &NoSystem);
        let (_, output, _) = run_in(
            &mut shell,
            &[
                "i=0; while [ $i -lt 40 ]; do mkdir a; cd a; i=$((i+1)); done; cd /",
                "ls -R /a | tail -n 1 | wc -l; find /a | wc -l; du -s /a > /dev/null; \
                 tree /a > /dev/null; cp -r /a /b; rm -r /a /b; ls /",
            ],
            b"",
        );
        assert_eq!(output, "1\n40\ndev\ntmp\n");
    });
}

// ----- SH-6: covered in console.rs -----
// ----- SH-7: cancellation -----

/// Cancels after a given number of polls.
struct CancellingHost {
    polls_left: u32,
}

impl Host for CancellingHost {
    fn run_external(
        &mut self,
        _name: &str,
        _args: &[String],
        _stdin: &mut dyn Input,
        _stdout: &mut dyn Output,
        _stderr: &mut dyn Output,
        _background: bool,
    ) -> Option<i32> {
        None
    }

    fn poll_cancel(&mut self) -> bool {
        self.polls_left = self.polls_left.saturating_sub(1);
        self.polls_left == 0
    }
}

fn run_cancelled(line: &str, polls: u32) -> (i32, String) {
    let mut shell = Shell::new(None, &NoSystem);
    let mut host = CancellingHost { polls_left: polls };
    let mut input = BytesInput::empty();
    let mut stdout = BytesOutput::new();
    let mut stderr = BytesOutput::new();
    let mut io = Io {
        stdin: &mut input,
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    let code = shell.execute_line(line, &mut io, &mut host);
    assert!(!shell.cancel_token().is_cancelled(), "the flag must be consumed");
    // The shell works again afterward.
    let mut plain = NoHost;
    assert_eq!(shell.execute_line("true", &mut io, &mut plain), 0);
    (code, String::from_utf8_lossy(stdout.bytes()).into_owned())
}

#[test]
fn cancellation_stops_endless_loops() {
    assert_eq!(run_cancelled("while :; do :; done", 500).0, 130);
    assert_eq!(run_cancelled("while :; do while :; do :; done; done", 500).0, 130);
    assert_eq!(run_cancelled("until false; do :; done", 500).0, 130);
    assert_eq!(run_cancelled("for ((;;)); do :; done", 500).0, 130);
    assert_eq!(run_cancelled("f() { while :; do :; done; }; f", 500).0, 130);
    assert_eq!(run_cancelled("x=$(while :; do :; done); echo no", 500).0, 130);
    assert_eq!(run_cancelled("(while :; do :; done); echo no", 500).0, 130);
    assert_eq!(run_cancelled("while :; do :; done | cat; echo no", 500).0, 130);
    assert_eq!(run_cancelled("sh -c 'while :; do :; done'; echo no", 500).0, 130);
    let (code, output) = run_cancelled("while :; do :; done; echo no", 50);
    assert_eq!(code, 130);
    assert_eq!(output, "");
}

#[test]
fn cancellation_stops_coreutils() {
    // `seq` writes in small pieces, so the streams notice the request.
    let (code, output) = run_cancelled("seq 1000000", 8);
    assert_eq!(code, 130);
    assert!(output.len() < 100_000);
    assert_eq!(run_cancelled("yes", 3).0, 130);
}

#[test]
fn a_token_can_be_held_by_another_task() {
    let shell = Shell::new(None, &NoSystem);
    let token: CancelToken = shell.cancel_token();
    let copy = token.clone();
    assert!(!token.is_cancelled());
    copy.cancel();
    assert!(token.is_cancelled());
}

/// A clock that moves forward each time it is read.
struct TickingSystem(AtomicU64);

impl System for TickingSystem {
    fn uptime_ms(&self) -> u64 {
        self.0.fetch_add(10, Ordering::Relaxed)
    }
}

#[test]
fn timeout_stops_shell_loops_and_reports_124() {
    let system = TickingSystem(AtomicU64::new(0));
    let mut shell = Shell::new(None, &system);
    let (code, _, _) = run_in(
        &mut shell,
        &["timeout 1 sh -c 'while :; do :; done'; echo $?"],
        b"",
    );
    assert_eq!(code, 0);
    let (_, output, _) = run_in(&mut shell, &["timeout 1 sh -c 'while :; do :; done'; echo $?"], b"");
    assert_eq!(output, "124\n");
    let (_, output, _) = run_in(&mut shell, &["timeout 100 echo done; echo $?"], b"");
    assert_eq!(output, "done\n0\n");
    assert_eq!(run_in(&mut shell, &["timeout"], b"").0, 125);
    assert_eq!(run_in(&mut shell, &["timeout x true"], b"").0, 125);
}

// ----- SH-8 to SH-18 -----

#[test]
fn assignments_are_not_split_or_globbed() {
    assert_eq!(out("A='hello world'; B=$A; echo \"[$B]\""), "[hello world]\n");
    let (_, output, _) = r(&["touch /p1 /p2", "x=/p*; echo \"$x\""]);
    assert_eq!(output, "/p*\n");
}

#[test]
fn substitution_status_reaches_the_shell() {
    assert_eq!(out("x=$(false); echo $?"), "1\n");
    assert_eq!(out("x=$(true); echo $?"), "0\n");
    assert_eq!(out("if x=$(false); then echo y; else echo n; fi"), "n\n");
    assert_eq!(status("x=$(exit 7)"), 7);
    assert_eq!(out("true; echo $(false) $?"), "1\n");
}

#[test]
fn arithmetic_expands_first_and_rejects_junk() {
    assert_eq!(out("n=5; echo $(($n + 1))"), "6\n");
    assert_eq!(out("n=5; echo \"$(($n + 1))\""), "6\n");
    assert_eq!(out("echo $((3 > 5))"), "0\n");
    assert_eq!(out("echo $((3 < 5))"), "1\n");
    assert_eq!(out("echo $(($(echo 2) * 3))"), "6\n");
    let (code, _, err) = r(&["echo $((1 +))"]);
    assert_ne!(code, 0);
    assert!(err.contains("syntax"), "{err}");
    let (code, _, err) = r(&["echo $((1 / 0))"]);
    assert_eq!(code, 1);
    assert!(err.contains("division"), "{err}");
}

#[test]
fn subshells_parse_and_restore_state() {
    assert_eq!(out("(echo hi)"), "hi\n");
    assert_eq!(out("x=1; (x=2; echo $x); echo $x"), "2\n1\n");
    assert_eq!(out("mkdir /d; (cd /d; pwd); pwd"), "/d\n/\n");
    assert_eq!(out("(exit 3); echo $?"), "3\n");
    assert_eq!(out("f() { echo o; }; (f() { echo i; }; f); f"), "i\no\n");
    assert_eq!(out("alias q='echo a'\n(alias q='echo b')\nq"), "a\n");
    assert_eq!(out("(echo a; echo b) | wc -l").trim(), "2");
    assert_eq!(out("( (echo nested) )"), "nested\n");
}

#[test]
fn exit_in_a_child_shell_stays_there() {
    assert_eq!(out("sh -c 'exit 3'; echo still"), "still\n");
    assert_eq!(out("sh -c 'exit 3'; echo $?"), "3\n");
    assert_eq!(out("bash -c 'exit 4'; echo $?"), "4\n");
    let mut shell = Shell::new(None, &NoSystem);
    run_in(&mut shell, &["sh -c 'exit 3'"], b"");
    assert_eq!(shell.take_exit_status(), None);
    run_in(&mut shell, &["exit 5"], b"");
    assert_eq!(shell.take_exit_status(), Some(5));
}

#[test]
fn wc_has_no_name_for_implicit_stdin() {
    assert_eq!(out("echo a | wc -l").trim(), "1");
    assert_eq!(out("echo a | wc -l -").trim(), "1 -");
    assert_eq!(status("test $(printf 'a\\nb\\n' | wc -l) -eq 2"), 0);
}

#[test]
fn dev_null_and_tmp_exist() {
    let (_, _, err) = r(&["ls nope 2>/dev/null"]);
    assert_eq!(err, "");
    assert_eq!(status("if echo x >/dev/null; then true; else false; fi"), 0);
    assert_eq!(out("echo x > /tmp/f; cat /tmp/f"), "x\n");
    assert_eq!(status("cat /dev/null"), 0);
    assert_eq!(status("[ -e /dev/null ]"), 0);
    assert_eq!(out("echo hi >/dev/stdout"), "hi\n");
    let (_, _, err) = r(&["echo hi >/dev/stderr"]);
    assert_eq!(err, "hi\n");
    assert_eq!(out("head -c 3 </dev/zero | od -A n -t x1").trim(), "00 00 00");
}

#[test]
fn basename_and_dirname_work() {
    assert_eq!(out("basename /a/b"), "b\n");
    assert_eq!(out("basename /a/b.txt .txt"), "b\n");
    assert_eq!(out("basename /a/b/"), "b\n");
    assert_eq!(out("basename /"), "/\n");
    assert_eq!(out("dirname /a/b"), "/a\n");
    assert_eq!(out("dirname a"), ".\n");
    assert_eq!(out("dirname /a"), "/\n");
    assert_eq!(out("dirname a/b/"), "a\n");
}

#[test]
fn quoted_glob_characters_do_not_glob() {
    let (code, _, _) = r(&["touch fooa foob; rm foo\\*; ls fooa foob"]);
    assert_eq!(code, 0);
    let (code, _, _) = r(&["touch fooa foob; rm 'foo*'; ls fooa foob"]);
    assert_eq!(code, 0);
    let (code, _, _) = r(&["touch fooa foob; rm \"foo*\"; ls fooa foob"]);
    assert_eq!(code, 0);
    assert_eq!(out("touch /fa /fb; echo /f\"*\""), "/f*\n");
    assert_eq!(out("touch /fa /fb; echo /f*"), "/fa /fb\n");
    assert_eq!(out("touch /fa /fb; x='*'; echo /f\"$x\""), "/f*\n");
    assert_eq!(out("touch /fa /fb; x='*'; echo /f$x"), "/fa /fb\n");
    assert_eq!(out("touch '/a?b' /axb; echo /a\\?*"), "/a?b\n");
}

#[test]
fn positional_parameters_work() {
    assert_eq!(out("echo 'echo $1' > /s; sh /s hi"), "hi\n");
    assert_eq!(out("echo 'echo $# $0 $2' > /s; sh /s a b c"), "3 /s b\n");
    assert_eq!(out("echo 'for a; do echo \"[$a]\"; done' > /s; sh /s 'x y' z"), "[x y]\n[z]\n");
    assert_eq!(out("set -- a b c; echo $# $2; shift; echo $1 $#; shift 2; echo $#"), "3 b\nb 2\n0\n");
    assert_eq!(status("set -- a; shift 2"), 1);
    assert_eq!(out("set -- 'a b' c; for x in \"$@\"; do echo \"<$x>\"; done"), "<a b>\n<c>\n");
    assert_eq!(out("set -- 'a b' c; for x in $@; do echo \"<$x>\"; done"), "<a>\n<b>\n<c>\n");
    assert_eq!(out("set -- 'a b' c; for x in \"$*\"; do echo \"<$x>\"; done"), "<a b c>\n");
    assert_eq!(out("set -- a b c d e f g h i j k; echo $10 ${10} ${11}"), "a0 j k\n");
    assert_eq!(out("set --; for x in \"$@\"; do echo no; done; echo \"$#\""), "0\n");
    assert_eq!(out("set -- a; printf '%s|' \"x$@y\"; echo"), "xay|\n");
    assert_eq!(out("set -- a b; printf '%s|' \"x$@y\"; echo"), "xa|by|\n");
    assert_eq!(out("sh -c 'echo $0 $1' zero one"), "zero one\n");
    assert_eq!(out("echo 'echo $1' > /s; . /s arg; set -- own; . /s; echo $1"), "arg\nown\nown\n");
}

#[test]
fn interactive_entry_detects_incomplete_commands() {
    use crate::lexer::needs_more;
    assert!(needs_more("for i in 1 2; do"));
    assert!(needs_more("echo 'open"));
    assert!(needs_more("if true"));
    assert!(!needs_more("echo ok"));
}

// ----- SH-19 to SH-24 -----

#[test]
fn parameter_operators_work() {
    assert_eq!(out("foo=a; echo ${X:-$foo}"), "a\n");
    assert_eq!(out("X=; echo ${X:-d} ${X-e}."), "d .\n");
    assert_eq!(out("echo ${U:=v} $U"), "v v\n");
    assert_eq!(out("X=s; echo ${X:+alt} ${Y:+alt}."), "alt .\n");
    assert_eq!(out("x=/a/b.tar.gz; echo ${x##*/} ${x%%.*} ${x%.*} ${x#*/}"), "b.tar.gz /a/b /a/b.tar a/b.tar.gz\n");
    assert_eq!(out("x=abcabc; echo ${x/b/X} ${x//b/X} ${x/#a/X} ${x/%c/X}"), "aXcabc aXcaXc Xbcabc abcabX\n");
    assert_eq!(out("x=hello; echo ${x:1:3} ${x: -2} ${x:2} ${#x}"), "ell lo llo 5\n");
    assert_eq!(out("x=hello; echo ${x^} ${x^^} ${x^^l}; y=HI; echo ${y,} ${y,,}"), "Hello HELLO heLLo\nhI hi\n");
    assert_eq!(out("x='a b'; printf '<%s>' ${U:-$x} \"${U:-$x}\" ${U:-\"$x\"}; echo"), "<a><b><a b><a b>\n");
    assert_eq!(out("x=y; y=z; echo ${!x}"), "z\n");
    assert_eq!(out("set -- a b c d; echo ${@:2:2} ${#@} ${*:3}"), "b c 4 c d\n");
    assert_eq!(out("echo ${X:-${Y:-${Z:-deep}}}"), "deep\n");
    assert_eq!(out("echo \"${X:-a}\" \"${X:-'q'}\""), "a 'q'\n");
    assert_eq!(out("x=a.b; echo ${x%.*}.c \"${x%\".b\"}\""), "a.c a\n");
    assert_eq!(out("echo ${X:-$(echo sub)}"), "sub\n");
    assert_eq!(out("x='*'; y=a; echo ${y/$x/z}; echo ${y/\"$x\"/z}"), "z\na\n");
}

#[test]
fn parameter_errors_are_reported() {
    let (code, output, err) = r(&["echo ${X:?oops}; echo no"]);
    assert_ne!(code, 0);
    assert_eq!(output, "");
    assert!(err.contains("oops"), "{err}");
    let (code, _, err) = r(&["echo ${X:?}"]);
    assert_ne!(code, 0);
    assert!(err.contains("null or not set"), "{err}");
    let (code, _, err) = r(&["echo ${x;y}"]);
    assert_ne!(code, 0);
    assert!(err.contains("bad substitution"), "{err}");
    let (code, _, _) = r(&["echo ${}"]);
    assert_ne!(code, 0);
    assert_eq!(out("X=1; echo ${X:?oops}"), "1\n");
    // A script stops on `${X:?}`; a prompt only ends the line.
    assert_eq!(out("sh -c 'echo ${X:?m}; echo no'; echo yes"), "yes\n");
}

#[test]
fn substitutions_do_not_change_the_parent() {
    assert_eq!(out("mkdir /docs; echo $(cd /docs; pwd); pwd"), "/docs\n/\n");
    assert_eq!(out("x=1; echo $(x=2; echo $x); echo $x"), "2\n1\n");
    assert_eq!(out("echo $(exit 3; echo no); echo after"), "\nafter\n");
    assert_eq!(out("f() { echo $(return 3; echo no); echo x; }; f"), "\nx\n");
}

#[test]
fn control_flow_does_not_leak() {
    let mut shell = Shell::new(None, &NoSystem);
    let (code, _, err) = run_in(&mut shell, &["return"], b"");
    assert_eq!(code, 1);
    assert!(err.contains("return"), "{err}");
    let (_, output, _) = run_in(&mut shell, &["echo a; echo b"], b"");
    assert_eq!(output, "a\nb\n");

    let (_, output, _) = run_in(&mut shell, &["break; echo after"], b"");
    assert_eq!(output, "after\n");
    assert_eq!(out("for i in 1 2; do break 5; done; echo a; echo b"), "a\nb\n");
    assert_eq!(out("for i in 1 2; do break && echo x; echo y; done; echo z"), "z\n");
    assert_eq!(
        out("for i in 1 2; do for j in a b; do echo $i$j; continue 2; done; done"),
        "1a\n2a\n"
    );
    assert_eq!(out("for i in 1 2 3; do [ $i = 2 ] && continue; echo $i; done"), "1\n3\n");
    assert_eq!(
        out("for i in 1 2; do for j in 3 4; do break 2; done; echo no; done; echo end"),
        "end\n"
    );
    assert_eq!(out("while true; do break; done; echo $?"), "0\n");
    assert_eq!(out("f() { for i in 1 2; do return 4; done; echo no; }; f; echo $?"), "4\n");
    assert_eq!(out("f() { break; echo in; }; for i in 1 2; do f; echo $i; done"), "in\n1\nin\n2\n");
}

#[test]
fn head_and_tail_counts_work() {
    let input = "printf 'a\\nb\\nc\\nd\\n' |";
    assert_eq!(out(&alloc::format!("{input} tail -n +2")), "b\nc\nd\n");
    assert_eq!(out(&alloc::format!("{input} tail -n 2")), "c\nd\n");
    assert_eq!(out(&alloc::format!("{input} tail -2")), "c\nd\n");
    assert_eq!(out(&alloc::format!("{input} head -n -1")), "a\nb\nc\n");
    assert_eq!(out(&alloc::format!("{input} head -2")), "a\nb\n");
    assert_eq!(out(&alloc::format!("{input} tail -c 2")), "d\n");
    assert_eq!(out(&alloc::format!("{input} tail -c +7")), "d\n");
    assert_eq!(out(&alloc::format!("{input} head -c 1")), "a");
    assert_eq!(out(&alloc::format!("{input} head -c -2")), "a\nb\nc\n");
    let (code, output, err) = r(&["printf 'a\\n' | head -n abc"]);
    assert_eq!((code, output.as_str()), (1, ""));
    assert!(err.contains("invalid number"), "{err}");
}

#[test]
fn sort_keys_and_errors_work() {
    let text = "printf 'b 2 x\\na 3 y\\nc 1 z\\n'";
    assert_eq!(out(&alloc::format!("{text} | sort -k 2")), "c 1 z\nb 2 x\na 3 y\n");
    assert_eq!(out(&alloc::format!("{text} | sort -k2")), "c 1 z\nb 2 x\na 3 y\n");
    assert_eq!(out(&alloc::format!("{text} | sort -k3,3r")), "c 1 z\na 3 y\nb 2 x\n");
    assert_eq!(out(&alloc::format!("{text} | sort -k 2,2nr")), "a 3 y\nb 2 x\nc 1 z\n");
    assert_eq!(out("printf 'a,2\\nb,1\\n' | sort -t, -k2"), "b,1\na,2\n");
    assert_eq!(out("printf 'a 1 z\\nb 1 y\\n' | sort -k 2,2 -k 3,3"), "b 1 y\na 1 z\n");
    assert_eq!(out("printf 'a 1 z\\nb 1 y\\n' | sort -s -k 2,2"), "a 1 z\nb 1 y\n");
    assert_eq!(out("printf '10\\n9\\n' | sort -n"), "9\n10\n");
    assert_eq!(out("printf '1K\\n2M\\n3\\n' | sort -h"), "3\n1K\n2M\n");
    assert_eq!(out("printf 'B\\na\\n' | sort -f"), "a\nB\n");
    assert_eq!(out("printf 'a\\nb\\na\\n' | sort -u"), "a\nb\n");
    assert_eq!(out("printf 'x\\n' | sort -o /o; cat /o"), "x\n");
    assert_eq!(status("sort /nope"), 2);
    assert_eq!(out("sort /nope 2>/dev/null; echo $?"), "2\n");
    assert_eq!(status("printf 'a\\nb\\n' | sort -c"), 0);
    assert_eq!(status("printf 'b\\na\\n' | sort -c"), 1);
    assert_eq!(status("echo a | sort -k 0"), 2);
}

#[test]
fn posix_utility_divergences_are_fixed() {
    assert_eq!(out("echo a,b | cut -d, -f1"), "a\n");
    assert_eq!(out("echo a,b,c | cut -d , -f 2-"), "b,c\n");
    assert_eq!(out("echo a,b,c | cut -d, -f3,1"), "a,c\n");
    assert_eq!(out("echo abc | cut -c2-"), "bc\n");
    assert_eq!(out("echo abc | cut -d, -f1"), "abc\n");
    assert_eq!(out("echo abc | cut -d, -sf1"), "");
    assert_eq!(status("echo a | cut -f 0"), 1);
    assert_eq!(out("echo hello | tr '[:lower:]' '[:upper:]'"), "HELLO\n");
    assert_eq!(out("echo hello | tr a-y b-z"), "ifmmp\n");
    assert_eq!(out("echo 'a1 b2' | tr -d '[:digit:]'"), "a b\n");
    assert_eq!(out("echo aabb | tr -s ab"), "ab\n");
    assert_eq!(out("echo abc | tr -c b x | tr -d '\\n'"), "xbxx");
    assert_eq!(out("seq 3 -1 1"), "3\n2\n1\n");
    assert_eq!(out("seq -s, 1 3"), "1,2,3\n");
    assert_eq!(out("seq -w 8 10"), "08\n09\n10\n");
    assert_eq!(out("seq 2 1"), "");
    assert_eq!(out("expr 2 + 3 '*' 4"), "14\n");
    assert_eq!(out("expr '(' 2 + 3 ')' '*' 4"), "20\n");
    assert_eq!(out("expr abc : 'a.'"), "2\n");
    assert_eq!(out("expr abc : '\\(a.\\)'"), "ab\n");
    assert_eq!(out("expr 0 '|' 5"), "5\n");
    assert_eq!(out("expr 1 '&' 0"), "0\n");
    assert_eq!(status("expr 1 '&' 0"), 1);
    assert_eq!(out("expr a = a"), "1\n");
    assert_eq!(out("expr length abc"), "3\n");
    assert_eq!(status("[ -s /missing ]"), 1);
    assert_eq!(status("[ abc -eq 0 ]"), 2);
    assert_eq!(status("[ 1 -eq 1 ]"), 0);
    assert_eq!(status("[ a ]"), 0);
    assert_eq!(status("[ ]"), 1);
    assert_eq!(status("[ a"), 2);
    assert_eq!(status("[ -n ]"), 0);
    assert_eq!(status("[ ! a ]"), 1);
    assert_eq!(status("[ a = a -a b = b ]"), 0);
    assert_eq!(status("[ '(' a ')' ]"), 0);
    assert_eq!(status("test -f = -f"), 0);
    assert_eq!(out("printf 'a\\nb\\nc\\n' | grep -E 'a|c'"), "a\nc\n");
    assert_eq!(out("echo a1 | grep '[[:digit:]]'"), "a1\n");
    assert_eq!(out("echo ab | grep -c '[[:digit:]]'"), "0\n");
}

// ----- SH-26 to SH-35 -----

#[test]
fn aliases_run_whole_command_lists() {
    assert_eq!(
        out("mkdir /docs; touch /docs/f\nalias x='cd /docs && ls'\nx"),
        "f\n"
    );
    assert_eq!(out("alias e='echo a; echo b'\ne"), "a\nb\n");
    assert_eq!(out("alias ls='ls -1'\nmkdir /q; touch /q/z; ls /q"), "z\n");
    assert_eq!(out("alias n='echo x | wc -l'\nn").trim(), "1");
    assert_eq!(out("alias g='if true; then echo y; fi'\ng"), "y\n");
    assert_eq!(out("alias a='b'; alias b='a'\na 2>/dev/null; echo done"), "done\n");
    assert_eq!(out("alias k='echo '\nalias v=value\nk v"), "value\n");
    assert_eq!(out("alias z='echo'\nFOO=1 z hi"), "hi\n");
    // Not a command position.
    assert_eq!(out("alias v=value\necho v; for v in v; do echo $v; done"), "v\nv\n");
    // An alias defined on one line works on the next line of a script.
    assert_eq!(out("printf 'alias y=\"echo z\"\\ny\\n' > /s; . /s"), "z\n");
}

#[test]
fn mv_refuses_to_replace_a_file_with_a_directory() {
    assert_eq!(out("mkdir /d; echo x > /f; mv /d /f 2>/dev/null; cat /f"), "x\n");
    assert_eq!(status("mkdir /d; echo x > /f; mv /d /f"), 1);
    assert_eq!(out("echo x > /f; echo y > /g; mv /f /g; cat /g"), "x\n");
}

#[test]
fn globs_treat_directories_and_dots_correctly() {
    assert_eq!(out("mkdir /t; cd /t; mkdir d; touch f; echo */"), "d/\n");
    assert_eq!(out("mkdir /t; cd /t; mkdir d; touch f; echo d/../*"), "d/../d d/../f\n");
    assert_eq!(out("mkdir /t; cd /t; touch a ab ac; echo a[!b]*"), "ac\n");
    assert_eq!(out("mkdir /t; cd /t; touch .h v; echo *; echo .h*"), "v\n.h\n");
    assert_eq!(out("mkdir /t; cd /t; echo nomatch*"), "nomatch*\n");
    assert_eq!(out("mkdir /t; cd /t; mkdir x y; touch x/1 y/2; echo */[0-9]"), "x/1 y/2\n");
    assert_eq!(out("mkdir /t; cd /t; touch a; set -f; echo *"), "*\n");
}

#[test]
fn redirections_follow_source_order_and_stop_on_errors() {
    assert_eq!(out("mkdir /docs; cd /docs > log; pwd; cd /; cat log | wc -c").trim(), "/docs\n0");
    assert_eq!(status("echo hi > /nope/x"), 1);
    assert_eq!(out("echo hi > /nope/x 2>/dev/null; echo $?"), "1\n");
    assert_eq!(out("touch /m; echo run > /nope/x 2>/dev/null"), "");
    let (code, _, err) = r(&["echo x > $EMPTY"]);
    assert_eq!(code, 1);
    assert!(err.contains("ambiguous"), "{err}");
    let (_, output, _) = r(&["cat /missing 2>&1 > /o; cat /o | wc -c"]);
    assert!(output.starts_with("cat:"), "{output}");
    let (_, output, err) = r(&["cat /missing > /o 2>&1; cat /o"]);
    assert_eq!(err, "");
    assert!(output.contains("missing"));
    assert_eq!(out("echo a >&2 2>/dev/null"), "");
    let (_, output, err) = r(&["echo a 2>/dev/null >&2"]);
    assert_eq!((output.as_str(), err.as_str()), ("", ""));
    assert_eq!(status("echo x >&5"), 1);
    assert_eq!(out("echo hi &> /b; cat /b"), "hi\n");
    assert_eq!(out("echo hi >/b; echo more &>>/b; cat /b"), "hi\nmore\n");
    assert_eq!(out("echo x > /f; cat < /f"), "x\n");
    assert_eq!(status("cat < /missing"), 1);
    assert_eq!(out("> /new; [ -f /new ] && echo yes"), "yes\n");
    assert_eq!(out("set -C; echo a > /k; echo b > /k 2>/dev/null; echo $?; echo c >| /k; cat /k"), "1\nc\n");
    assert_eq!(out("x=/o; echo hi > $x; cat /o"), "hi\n");
    let (code, _, _) = r(&["f='a b'; echo hi > $f"]);
    assert_eq!(code, 1);
}

#[test]
fn shell_misc_fixes_work() {
    assert_eq!(status("foo | cat"), 0);
    assert_eq!(status("cat /dev/null | foo"), 127);
    assert_eq!(status("foo"), 127);
    let mut shell = Shell::new(None, &NoSystem);
    let (_, output, _) = run_in(&mut shell, &["sh"], b"echo hi\n");
    assert_eq!(output, "hi\n");
    assert_eq!(out("echo 'echo piped' | sh"), "piped\n");
    assert_eq!(out("FOO=0; FOO=1 echo $FOO"), "0\n");
    assert_eq!(out("FOO=1 sh -c 'echo $FOO'; echo \"[$FOO]\""), "1\n[]\n");
    assert_eq!(out("echo ~foo ~ ~/x"), "~foo / //x\n");
    assert!(!out("false; env").contains("?="));
    for name in ["sh", "true", ":", "bash", "false"] {
        assert!(crate::builtin_names().contains(&name));
    }
}

#[test]
fn trailing_slashes_and_removed_directories_are_errors() {
    assert_eq!(status("touch /new/"), 1);
    assert_eq!(status("echo x > /f; cat /f/"), 1);
    assert_eq!(status("echo x > /f; touch /f/"), 1);
    assert_eq!(status("echo x > /f; rm /f/"), 1);
    assert_eq!(status("mkdir /d; touch /d/"), 0);
    assert_eq!(status("mkdir /d; cd /d; rm -r ."), 1);
    assert_eq!(out("mkdir -p /a/b; cd /a/b; rm -r /a; pwd; echo $PWD"), "/\n/\n");
    assert_eq!(out("mkdir -p /a/b; (cd /a/b; rm -r /a); pwd"), "/\n");
}

// ----- Language features -----

#[test]
fn functions_and_locals_work() {
    assert_eq!(out("f() { echo $1 $#; }; f a b"), "a 2\n");
    assert_eq!(out("function g { echo g; }; g"), "g\n");
    assert_eq!(out("function fh() { echo h; }; fh"), "h\n");
    assert_eq!(out("f() { return 3; }; f; echo $?"), "3\n");
    assert_eq!(out("f() { local x=in; echo $x; }; x=out; f; echo $x"), "in\nout\n");
    assert_eq!(out("f() { local x; x=in; }; f; echo \"[$x]\""), "[]\n");
    assert_eq!(out("f() { x=global; }; f; echo $x"), "global\n");
    assert_eq!(out("f() { local x=1; g; echo $x; }; g() { x=2; }; f"), "2\n");
    assert_eq!(status("local x=1"), 1);
    assert_eq!(out("fact() { if [ $1 -le 1 ]; then echo 1; else echo $(( $1 * $(fact $(($1 - 1))) )); fi; }; fact 5"), "120\n");
    assert_eq!(out("set -- o; f() { echo $1; }; f i; echo $1"), "i\no\n");
    assert_eq!(out("f() { echo fn; }; unset -f f; f 2>/dev/null; echo $?"), "127\n");
    assert_eq!(out("f() ( x=1 ); x=0; f; echo $x"), "0\n");
    assert_eq!(out("f() { echo hi; } > /o; f; cat /o"), "hi\n");
    assert_eq!(out("echo() { command echo wrapped $1; }; echo x"), "wrapped x\n");
    assert_eq!(out("f() { local a=$1 b=\"$2\"; echo \"$a|$b\"; }; f 'x y' 'p q'"), "x y|p q\n");
    assert_eq!(out("type f 2>/dev/null; f() { :; }; type f"), "f is a function\n");
}

#[test]
fn the_fd_table_handles_compound_redirection() {
    assert_eq!(
        out("printf 'a\\nb\\n' > /i; while read l; do echo \"<$l>\"; done < /i"),
        "<a>\n<b>\n"
    );
    assert_eq!(out("for i in 1 2; do echo $i; done > /o; cat /o"), "1\n2\n");
    assert_eq!(out("{ echo a; echo b; } | wc -l").trim(), "2");
    assert_eq!(out("{ echo a; echo b; } > /o; cat /o"), "a\nb\n");
    assert_eq!(out("if true; then echo y; fi > /o; cat /o"), "y\n");
    assert_eq!(out("(echo s) > /o; cat /o"), "s\n");
    assert_eq!(out("case a in a) echo c;; esac > /o; cat /o"), "c\n");
    let (_, output, err) = r(&["{ echo e >&2; echo o; } 2>/dev/null"]);
    assert_eq!((output.as_str(), err.as_str()), ("o\n", ""));
    assert_eq!(out("exec 3> /o; echo x >&3; echo y >&3; exec 3>&-; cat /o"), "x\ny\n");
    assert_eq!(out("exec 3> /o; echo x >&3; cat /o; exec 3>&-"), "x\n");
    assert_eq!(out("echo in > /i; exec 4< /i; read l <&4; echo $l"), "in\n");
    assert_eq!(out("printf 'a\\nb\\n' > /i; { read x; read y; } < /i; echo $y$x"), "ba\n");
    assert_eq!(out("exec 3>&1; echo out >&3"), "out\n");
    assert_eq!(status("echo hi >&-"), 0);
    assert_eq!(out("{ echo a > /o; cat /o; } > /p; cat /p"), "a\n");
    assert_eq!(out("exec > /o; echo hidden; exec >/dev/stderr"), "");
    assert_eq!(out("printf 'x\\n' | while read l; do echo got $l; done"), "got x\n");
    let (_, output, _) = r(&["seq 1 3 | while read n; do echo $n; done | tail -n 1"]);
    assert_eq!(output, "3\n");
    let (_, output, _) = r(&["ls /missing |& wc -l"]);
    assert_eq!(output.trim(), "1");
}

#[test]
fn here_documents_work() {
    assert_eq!(out("cat <<EOF\nhello\nEOF"), "hello\n");
    assert_eq!(out("x=1; cat <<EOF\nv=$x $((1+1)) $(echo s) \\$x\nEOF"), "v=1 2 s $x\n");
    assert_eq!(out("x=1; cat <<'EOF'\n$x\\n\nEOF"), "$x\\n\n");
    assert_eq!(out("x=1; cat <<\"E\"\n$x\nE"), "$x\n");
    assert_eq!(out("cat <<-E\n\ta\n\t\tb\n\tE"), "a\nb\n");
    assert_eq!(out("cat <<E | wc -l\na\nb\nE").trim(), "2");
    assert_eq!(out("cat <<A; cat <<B\nfirst\nA\nsecond\nB"), "first\nsecond\n");
    assert_eq!(out("cat <<E > /o\nto file\nE\ncat /o"), "to file\n");
    assert_eq!(out("while read l; do echo $l; done <<E\n1\n2\nE"), "1\n2\n");
    assert_eq!(out("f() { cat; }; f <<E\nx\nE"), "x\n");
    assert_eq!(out("cat <<E\nE"), "");
    assert_eq!(out("if true; then cat <<E\ninner\nE\nfi"), "inner\n");
    assert_eq!(out("cat <<< here"), "here\n");
    assert_eq!(out("x='a b'; cat <<< \"$x\""), "a b\n");
    assert_eq!(out("read a b <<< 'one two'; echo $b $a"), "two one\n");
    assert_eq!(out("echo $(cat <<E\nsub\nE\n)"), "sub\n");
}

#[test]
fn case_works() {
    assert_eq!(out("case abc in a*) echo y;; *) echo n;; esac"), "y\n");
    assert_eq!(out("case x in a|x) echo one;; esac"), "one\n");
    assert_eq!(out("case x in (x) echo paren;; esac"), "paren\n");
    assert_eq!(out("case x in y) echo n;; esac; echo $?"), "0\n");
    assert_eq!(out("x=abc; case $x in abc) echo v;; esac"), "v\n");
    assert_eq!(out("case 'a b' in 'a b') echo q;; esac"), "q\n");
    assert_eq!(out("case a in a) echo 1;& b) echo 2;; c) echo 3;; esac"), "1\n2\n");
    assert_eq!(out("case a in a) echo 1;;& a) echo 2;; *) echo 3;; esac"), "1\n2\n");
    assert_eq!(out("case a in\n  a)\n    echo nl\n    ;;\n  *)\n    echo no\n    ;;\nesac"), "nl\n");
    assert_eq!(out("case '*' in \\*) echo star;; esac"), "star\n");
    assert_eq!(out("case abc in \"a*\") echo no;; *) echo yes;; esac"), "yes\n");
    assert_eq!(out("x='a*'; case abc in $x) echo g;; esac; case abc in \"$x\") echo no;; esac"), "g\n");
    assert_eq!(out("case a in a) esac; echo ok"), "ok\n");
    assert_eq!(out("case a in a) echo last; esac"), "last\n");
    assert_eq!(out("case 7 in [0-9]) echo digit;; esac"), "digit\n");
    assert_eq!(out("for i in 1 2; do case $i in 1) continue;; esac; echo $i; done"), "2\n");
}

#[test]
fn arithmetic_commands_work() {
    assert_eq!(status("(( 1 < 2 ))"), 0);
    assert_eq!(status("(( 0 ))"), 1);
    assert_eq!(out("(( x = 3 + 4 )); echo $x"), "7\n");
    assert_eq!(out("x=1; ((x++)); ((x+=2)); echo $x"), "4\n");
    assert_eq!(out("let 'x=2*3' y=x+1; echo $x $y"), "6 7\n");
    assert_eq!(status("let 0"), 1);
    assert_eq!(out("i=0; while (( i < 2 )); do echo $i; ((i++)); done"), "0\n1\n");
    assert_eq!(out("for ((i=0;i<2;i++)); do echo $i; done"), "0\n1\n");
    assert_eq!(out("for ((;;)); do break; done; echo out"), "out\n");
    assert_eq!(out("if (( 2 > 1 )); then echo y; fi"), "y\n");
    assert_eq!(out("echo $((1 ? 2 : 3)) $((2**10)) $((7%3)) $((1<<4)) $((~0)) $((!1))"), "2 1024 1 16 -1 0\n");
    assert_eq!(out("echo $((0x10)) $((010)) $((2#11))"), "16 8 3\n");
    assert_eq!(out("a=2; b=3; echo $((a*b)) $(( a + b ))"), "6 5\n");
    assert_eq!(out("echo $(( (1+2)*3 ))"), "9\n");
    assert_eq!(out("(( 1 / 0 )); echo $?"), "1\n");
    assert_eq!(out("x=5; ((x > 3)) && echo big"), "big\n");
    assert_eq!(status("((a);(b)) 2>/dev/null"), 127);
}

#[test]
fn conditional_expressions_work() {
    assert_eq!(status("[[ abc == a* ]]"), 0);
    assert_eq!(status("[[ abc == 'a*' ]]"), 1);
    assert_eq!(status("[[ abc != a* ]]"), 1);
    assert_eq!(status("x='a b'; [[ $x == 'a b' ]]"), 0);
    assert_eq!(status("[[ -z '' && -n a ]]"), 0);
    assert_eq!(status("[[ -f /nope || a == a ]]"), 0);
    assert_eq!(status("[[ ! a == b ]]"), 0);
    assert_eq!(status("[[ ( a == a ) && ( b == b ) ]]"), 0);
    assert_eq!(status("[[ abc =~ ^a(b|x)c$ ]]"), 0);
    assert_eq!(status("[[ abc =~ z ]]"), 1);
    assert_eq!(status("[[ a < b ]]"), 0);
    assert_eq!(status("[[ b > a ]]"), 0);
    assert_eq!(status("[[ 2 -lt 10 ]]"), 0);
    assert_eq!(status("x=3; [[ x+1 -eq 4 ]]"), 0);
    assert_eq!(status("[[ -d / ]]"), 0);
    assert_eq!(status("[[ -e /nope ]]"), 1);
    assert_eq!(status("[[ a ]]"), 0);
    assert_eq!(status("[[ '' ]]"), 1);
    assert_eq!(status("x=1; [[ -v x ]]"), 0);
    assert_eq!(out("if [[ a == a ]]; then echo y; fi"), "y\n");
    assert_eq!(out("[[ a == b ]] || echo no"), "no\n");
    assert_eq!(out("touch /g1; [[ /g* == /g1 ]] || echo noglob"), "noglob\n");
    let (code, _, _) = r(&["[[ a =~ ( ]]"]);
    assert_eq!(code, 2);
}

#[test]
fn quoting_forms_work() {
    assert_eq!(out("echo `echo back`"), "back\n");
    assert_eq!(out("echo \"`echo a b`\""), "a b\n");
    assert_eq!(out("echo `echo \\`echo in\\``"), "in\n");
    assert_eq!(out("x=`echo v`; echo $x"), "v\n");
    assert_eq!(out("echo $'a\\tb\\n'x"), "a\tb\nx\n");
    assert_eq!(out("echo $'it\\'s' $'\\x41\\101\\u00e9'"), "it's AA\u{e9}\n");
    assert_eq!(out("echo a\\ b \"c\\\"d\" 'e\\f'"), "a b c\"d e\\f\n");
    assert_eq!(out("echo \"\\$x \\` \\\\ \\a\""), "$x ` \\ \\a\n");
    assert_eq!(out("echo a\\\nb"), "ab\n");
    assert_eq!(out("printf '%s|' '' \"\" x; echo"), "||x|\n");
}

#[test]
fn assignment_forms_work() {
    assert_eq!(out("x=a; x+=b; echo $x"), "ab\n");
    assert_eq!(out("u+=z; echo $u"), "z\n");
    assert_eq!(out("x=~/d; echo $x"), "//d\n");
    assert_eq!(out("export E=1; sh -c 'echo $E'; F=2; sh -c 'echo \"[$F]\"'"), "1\n[]\n");
    assert_eq!(out("X=1 Y=$X; echo $Y"), "1\n");
    assert_eq!(out("export a='x y'; printenv a"), "x y\n");
    assert_eq!(out("a=1; export a; env | grep -c '^a=1$'"), "1\n");
    assert_eq!(out("env Q=7 printenv Q"), "7\n");
    assert_eq!(out("export -n HOME; env | grep -c '^HOME='"), "0\n");
}

#[test]
fn ifs_controls_splitting() {
    assert_eq!(out("IFS=,; x=a,b,,c; printf '<%s>' $x; echo"), "<a><b><><c>\n");
    assert_eq!(out("IFS=,; x=',a'; printf '<%s>' $x; echo"), "<><a>\n");
    assert_eq!(out("IFS=,; x='a,'; printf '<%s>' $x; echo"), "<a>\n");
    assert_eq!(out("IFS=; x='a b'; printf '<%s>' $x; echo"), "<a b>\n");
    assert_eq!(out("IFS=' ,'; x='a , b'; printf '<%s>' $x; echo"), "<a><b>\n");
    assert_eq!(out("x='  a   b  '; printf '<%s>' $x; echo"), "<a><b>\n");
    assert_eq!(out("IFS=:; set -- a b; echo \"$*\""), "a:b\n");
    assert_eq!(out("IFS=; set -- a b; echo \"$*\""), "ab\n");
    assert_eq!(out("unset IFS; x='a b'; printf '<%s>' $x; echo"), "<a><b>\n");
    assert_eq!(out("IFS=: read a b <<< 'x:y z'; echo \"$a|$b\""), "x|y z\n");
    assert_eq!(out("printf '<%s>' $(echo a b) \"$(echo c d)\"; echo"), "<a><b><c d>\n");
    assert_eq!(out("x=; printf '<%s>' a$x b \"$x\"; echo"), "<a><b><>\n");
    assert_eq!(out("x=''; printf '<%s>' $x; echo"), "<>\n");
}

#[test]
fn shell_options_work() {
    assert_eq!(out("sh -c 'set -e; false; echo no'; echo $?"), "1\n");
    assert_eq!(out("sh -c 'set -e; true; echo yes'"), "yes\n");
    assert_eq!(out("sh -c 'set -e; if false; then :; fi; echo a'"), "a\n");
    assert_eq!(out("sh -c 'set -e; false || true; echo b'"), "b\n");
    assert_eq!(out("sh -c 'set -e; false && true; echo c'"), "c\n");
    assert_eq!(out("sh -c 'set -e; ! false; echo d'"), "d\n");
    assert_eq!(out("sh -c 'set -e; while false; do :; done; echo e'"), "e\n");
    assert_eq!(out("sh -c 'set -e; true && false; echo no'; echo $?"), "1\n");
    assert_eq!(out("sh -c 'set -e; x=$(false); echo no'; echo $?"), "1\n");
    assert_eq!(out("sh -c 'set -e; (false); echo no'; echo $?"), "1\n");
    assert_eq!(out("sh -c 'set -e; f() { false; echo no; }; f'; echo $?"), "1\n");
    assert_eq!(out("sh -c 'set -e; set +e; false; echo g'"), "g\n");
    assert_eq!(out("sh -ec 'false; echo no'; echo $?"), "1\n");

    let (code, _, err) = r(&["sh -c 'set -u; echo $NOPE; echo no'"]);
    assert_eq!(code, 127);
    assert!(err.contains("unbound"), "{err}");
    assert_eq!(out("sh -c 'set -u; echo ${NOPE:-ok}'"), "ok\n");
    assert_eq!(out("sh -c 'set -u; set --; echo \"$@\"\"[$#]\"'"), "[0]\n");
    assert_eq!(out("set -u; X=1; echo $X"), "1\n");

    let (_, output, err) = r(&["set -x; echo traced; X=1 true"]);
    assert_eq!(output, "traced\n");
    assert!(err.contains("+ echo traced"), "{err}");
    assert!(err.contains("+ X=1 true"), "{err}");
    let (_, _, err) = r(&["PS4='>> '; set -x; true"]);
    assert!(err.contains(">> true"), "{err}");

    assert_eq!(status("false | true"), 0);
    assert_eq!(status("set -o pipefail; false | true"), 1);
    assert_eq!(status("set -o pipefail; true | true"), 0);
    assert_eq!(out("set -o pipefail; set +o pipefail; false | true; echo $?"), "0\n");
    assert_eq!(out("set -o noglob; touch /a; echo /*"), "/*\n");
    assert_eq!(out("set -ef; echo $-"), "ef\n");
    assert_eq!(status("set -Z"), 2);
    assert_eq!(status("set -o nope"), 2);
    assert_eq!(out("set a b; echo $2"), "b\n");
}

#[test]
fn readonly_works() {
    let (code, output, err) = r(&["readonly R=1; R=2; echo $R"]);
    assert_eq!(output, "1\n");
    assert_eq!(code, 0);
    assert!(err.contains("readonly"), "{err}");
    assert_eq!(status("readonly R=1; R=2"), 1);
    assert_eq!(status("readonly R=1; unset R"), 1);
    assert_eq!(status("readonly R=1; export R=2"), 1);
    assert_eq!(status("readonly R=1; (( R = 2 ))"), 1);
    assert_eq!(status("readonly R=1; for R in a; do :; done"), 1);
    assert_eq!(status("readonly R=1; read R <<< x"), 2);
    assert!(out("readonly R=1; readonly").contains("readonly R='1'"));
    assert_eq!(out("x=1; readonly x; echo $x"), "1\n");
}

#[test]
fn getopts_works() {
    let script = "while getopts ab:c opt; do case $opt in a) echo A;; b) echo B=$OPTARG;; c) echo C;; \\?) echo bad;; esac; done; shift $((OPTIND-1)); echo rest=$*";
    assert_eq!(
        out(&alloc::format!("set -- -a -b val -c x y; {script}")),
        "A\nB=val\nC\nrest=x y\n"
    );
    assert_eq!(
        out(&alloc::format!("set -- -ac -bval z; {script}")),
        "A\nC\nB=val\nrest=z\n"
    );
    assert_eq!(out(&alloc::format!("set -- -- -a; {script}")), "rest=-a\n");
    assert_eq!(out(&alloc::format!("set -- plain -a; {script}")), "rest=plain -a\n");
    assert_eq!(out(&alloc::format!("set -- -z 2>/dev/null; {script} 2>/dev/null")), "bad\nrest=\n");
    assert_eq!(
        out("getopts :b: o -z; echo \"$o $OPTARG\"; OPTIND=1; getopts :b: o -b; echo \"$o $OPTARG\""),
        "? z\n: b\n"
    );
    assert_eq!(out("getopts a o -a; echo $o $OPTIND"), "a 2\n");
    assert_eq!(status("getopts"), 2);
}

#[test]
fn scripts_run_by_path_and_from_path() {
    assert_eq!(out("echo 'echo ran $1' > /x.sh; /x.sh a"), "ran a\n");
    assert_eq!(out("mkdir /d; echo 'echo rel' > /d/r; cd /d; ./r"), "rel\n");
    assert_eq!(out("printf '#!/bin/sh\\necho shebang\\n' > /x; /x"), "shebang\n");
    assert_eq!(out("printf '#!/usr/bin/env bash\\necho env\\n' > /x; /x"), "env\n");
    assert_eq!(out("mkdir /bin; echo 'echo found' > /bin/tool; tool"), "found\n");
    assert_eq!(status("/missing"), 127);
    assert_eq!(status("mkdir /d; /d"), 126);
    assert_eq!(status("printf 'a\\0b' > /b; /b"), 126);
    assert_eq!(status("printf '#!/usr/bin/python\\n' > /p; /p"), 126);
    assert_eq!(out("echo 'x=child; exit 2' > /s; x=parent; /s; echo $? $x"), "2 parent\n");
    assert_eq!(out("f() { echo fn; }; echo 'f 2>/dev/null; echo $?' > /s; /s"), "127\n");
    assert_eq!(out("mkdir /bin; touch /bin/t; which t cat nope; echo $?"), "/bin/t\n/bin/cat\n1\n");
    assert_eq!(out("command -v cat; command -v cd; command -v nope; echo $?"), "/bin/cat\ncd\n1\n");
    assert_eq!(out("type cd cat 2>&1"), "cd is a shell builtin\ncat is a coreutils command\n");
}

#[test]
fn read_handles_ifs_flags_and_end_of_file() {
    let run_read = |line: &str, input: &[u8]| {
        let mut shell = Shell::new(None, &NoSystem);
        run_in(&mut shell, &[line], input)
    };
    assert_eq!(run_read("read a b; echo \"$a|$b\"", b"1 2 3\n").1, "1|2 3\n");
    assert_eq!(run_read("read a; echo \"[$a]\"", b"  x  \n").1, "[x]\n");
    assert_eq!(run_read("read; echo \"[$REPLY]\"", b"  x  \n").1, "[  x  ]\n");
    assert_eq!(run_read("read a; echo $?", b"").1, "1\n");
    assert_eq!(run_read("read a; echo $? $a", b"tail").1, "1 tail\n");
    assert_eq!(run_read("read -r a; echo \"$a\"", b"a\\b\n").1, "a\\b\n");
    assert_eq!(run_read("read a; echo \"$a\"", b"a\\b\n").1, "ab\n");
    assert_eq!(run_read("read a; echo \"$a\"", b"a\\\nb\n").1, "ab\n");
    assert_eq!(run_read("read -n 2 a; echo $a", b"abcd\n").1, "ab\n");
    assert_eq!(run_read("read -d , a; echo $a", b"x,y\n").1, "x\n");
    assert_eq!(run_read("read -p prompt a; echo $a", b"v\n").2, "prompt");
    assert_eq!(run_read("read 1bad", b"v\n").0, 2);
    assert_eq!(run_read("read a; cat", b"one\ntwo\n").1, "two\n");
}

// ----- Utilities -----

#[test]
fn small_utilities_work() {
    assert_eq!(out("printf 'abcdef' | fold -w 2"), "ab\ncd\nef");
    assert_eq!(out("printf 'a b c\\n' | fold -sw 3"), "a \nb c\n");
    assert_eq!(out("printf 'a\\nb\\n' > /1; printf 'b\\nc\\n' > /2; comm /1 /2"), "a\n\t\tb\n\tc\n");
    assert_eq!(out("printf 'a\\nb\\n' > /1; printf 'b\\nc\\n' > /2; comm -12 /1 /2"), "b\n");
    assert_eq!(out("seq 5 | split -l 2 - /p; cat /paa; cat /pac"), "1\n2\n5\n");
    assert_eq!(out("printf abcd | split -b 3 - /q; cat /qab"), "d");
    assert_eq!(out("printf hello | dd bs=1 skip=1 count=3 status=none"), "ell");
    assert_eq!(out("dd if=/dev/zero bs=2 count=2 status=none | wc -c").trim(), "4");
    assert_eq!(out("echo x > /in; dd if=/in of=/out status=none; cat /out"), "x\n");
    assert_eq!(out("printf 'ab\\0hello\\0xyz' | strings"), "hello\n");
    assert_eq!(out("printf 'ab\\0hi' | strings -n 2"), "ab\nhi\n");
    assert_eq!(out("mkdir /d; cd /d; realpath a/../b"), "/d/b\n");
    assert_eq!(status("realpath -e /nope"), 1);
    assert_eq!(out("x=$(mktemp); [ -f \"$x\" ] && echo ok; rm $x"), "ok\n");
    assert_eq!(out("x=$(mktemp -d); [ -d \"$x\" ] && echo ok"), "ok\n");
    assert_eq!(out("x=$(mktemp /fooXXX); case $x in /foo*) echo ok;; esac"), "ok\n");
    assert_eq!(status("mktemp nope"), 1);
    assert_eq!(out("echo hi > /t; file /t"), "/t: ASCII text\n");
    assert_eq!(out("printf '\\037\\213x' > /g; file -b /g"), "gzip compressed data\n");
    assert_eq!(out("file /tmp"), "/tmp: directory\n");
    assert_eq!(out("date -d @0 '+%Y-%m-%d %H:%M:%S'"), "1970-01-01 00:00:00\n");
    assert_eq!(out("date -d @86400 +%F"), "1970-01-02\n");
    assert!(out("date").contains("1970"));
    assert_eq!(out("printf 'ab' | od -A n -t x1"), " 61 62\n");
    assert_eq!(out("printf 'a' | od -c"), "0000000   a\n0000001\n");
    assert_eq!(out("printf 'a\\nb\\n' | od -A d -c | head -n 1"), "0000000   a  \\n   b  \\n\n");
}

#[test]
fn more_flags_work() {
    let text = "printf 'one\\ntwo\\nthree\\nfour\\nfive\\n'";
    assert_eq!(out(&alloc::format!("{text} | grep -o 'o.'")), "on\nou\n");
    assert_eq!(out(&alloc::format!("{text} | grep -m 1 o")), "one\n");
    assert_eq!(out(&alloc::format!("{text} | grep -A 1 two")), "two\nthree\n");
    assert_eq!(out(&alloc::format!("{text} | grep -B 1 two")), "one\ntwo\n");
    assert_eq!(out(&alloc::format!("{text} | grep -C 1 three")), "two\nthree\nfour\n");
    assert_eq!(out(&alloc::format!("{text} | grep -n -A 1 one")), "1:one\n2-two\n");
    assert_eq!(out(&alloc::format!("{text} | grep -c -v e")), "2\n");
    assert_eq!(out(&alloc::format!("{text} | grep -e one -e two")), "one\ntwo\n");
    assert_eq!(out(&alloc::format!("{text} | grep -x two")), "two\n");
    assert_eq!(out(&alloc::format!("{text} | grep -i TWO")), "two\n");
    assert_eq!(out(&alloc::format!("{text} | grep -F t.")), "");
    assert_eq!(out(&alloc::format!("{text} | grep -E '^(one|two)$'")), "one\ntwo\n");
    assert_eq!(out(&alloc::format!("{text} | grep 'o\\|e' | wc -l")).trim(), "5");
    assert_eq!(out("echo a > /f1; echo a > /f2; grep -l a /f1 /f2; grep -H a /f1; grep -h a /f1 /f2"), "/f1\n/f2\n/f1:a\na\na\n");
    assert_eq!(out("mkdir -p /r/s; echo needle > /r/s/f; grep -r needle /r"), "/r/s/f:needle\n");
    assert_eq!(status("grep -q x /missing"), 2);
    assert_eq!(status("grep -s x /missing"), 2);
    assert_eq!(status("grep '\\(' /dev/null"), 2);
    assert_eq!(status("grep"), 2);

    assert_eq!(out("printf 'a 1\\nb 1\\nc 2\\n' | uniq -f 1"), "a 1\nc 2\n");
    assert_eq!(out("printf 'xa\\nya\\nzb\\n' | uniq -s 1"), "xa\nzb\n");
    assert_eq!(out("printf 'ab\\nac\\nb\\n' | uniq -w 1"), "ab\nb\n");
    assert_eq!(out("printf 'a\\na\\nb\\n' | uniq -d"), "a\n");
    assert_eq!(out("printf 'a\\na\\nb\\n' | uniq -u"), "b\n");
    assert_eq!(out("printf 'a\\nA\\n' | uniq -i"), "a\n");
    assert_eq!(out("printf 'a\\na\\n' > /u; uniq /u /v; cat /v"), "a\n");

    assert_eq!(out("mkdir /l; cd /l; touch a; echo xx > b; echo xxxx > c; ls -S"), "c\nb\na\n");
    assert_eq!(out("mkdir /l; cd /l; touch .h v; ls -A; ls -a"), ".h\nv\n.\n..\n.h\nv\n");
    assert_eq!(out("mkdir /l; cd /l; touch a; mkdir d; ls -F"), "a\nd/\n");
    assert_eq!(out("mkdir /l; cd /l; touch b a; ls -r"), "b\na\n");
    assert_eq!(status("ls /nope"), 2);
    assert_eq!(out("touch /z1; ls /z1 /tmp; "), "/z1\n\n/tmp:\n");
    assert_eq!(status("ls -Q"), 2);
}

#[test]
fn xargs_works() {
    assert_eq!(out("printf 'a b\\nc\\n' | xargs"), "a b c\n");
    assert_eq!(out("printf 'a b\\nc\\n' | xargs -n 1 echo x"), "x a\nx b\nx c\n");
    assert_eq!(out("printf 'p\\nq\\n' | xargs -I {} echo [{}]"), "[p]\n[q]\n");
    assert_eq!(out("printf 'a b\\0c\\0' | xargs -0 -n 1"), "a b\nc\n");
    assert_eq!(out("printf 'a,b' | xargs -d , -n 1"), "a\nb\n");
    assert_eq!(out("printf '\"a b\" c' | xargs -n 1"), "a b\nc\n");
    assert_eq!(out("touch /x1 /x2; printf '/x1 /x2' | xargs rm; ls /x1 2>/dev/null; echo $?"), "2\n");
    assert_eq!(status("echo a | xargs nope 2>/dev/null"), 127);
    assert_eq!(status("echo a | xargs false"), 123);
    assert_eq!(out("printf '' | xargs -r echo run"), "");
    assert_eq!(out("printf '' | xargs echo run"), "run\n");
}

#[test]
fn diff_works() {
    assert_eq!(status("echo a > /1; echo a > /2; diff /1 /2"), 0);
    assert_eq!(out("echo a > /1; echo b > /2; diff /1 /2"), "1c1\n< a\n---\n> b\n");
    assert_eq!(out("printf 'a\\nb\\n' > /1; printf 'a\\n' > /2; diff /1 /2"), "2d1\n< b\n");
    assert_eq!(out("printf 'a\\n' > /1; printf 'a\\nb\\n' > /2; diff /1 /2"), "1a2\n> b\n");
    assert_eq!(
        out("printf 'a\\nb\\nc\\n' > /1; printf 'a\\nx\\nc\\n' > /2; diff -u /1 /2"),
        "--- /1\n+++ /2\n@@ -1,3 +1,3 @@\n a\n-b\n+x\n c\n"
    );
    assert_eq!(out("echo a > /1; echo b > /2; diff -q /1 /2"), "Files /1 and /2 differ\n");
    assert_eq!(status("diff /nope /nope2"), 2);
    assert_eq!(status("echo a > /1; echo b > /2; diff /1 /2"), 1);
    assert_eq!(status("diff"), 2);
}

#[test]
fn find_expressions_work() {
    let setup = "mkdir -p /f/a /f/b; echo x > /f/a/one.txt; touch /f/a/two.log /f/b/three.txt; cd /f; ";
    let run_find = |tail: &str| out(&alloc::format!("{setup}{tail}"));
    assert_eq!(run_find("find . -name '*.txt'"), "./a/one.txt\n./b/three.txt\n");
    assert_eq!(run_find("find . -iname '*.TXT' -type f | wc -l").trim(), "2");
    assert_eq!(run_find("find . -type d"), ".\n./a\n./b\n");
    assert_eq!(run_find("find . -maxdepth 1 -type d"), ".\n./a\n./b\n");
    assert_eq!(run_find("find . -mindepth 2 -name 't*'"), "./a/two.log\n./b/three.txt\n");
    assert_eq!(run_find("find . -name '*.log' -o -name 'one*'"), "./a/one.txt\n./a/two.log\n");
    assert_eq!(run_find("find . ! -name '*.txt' -type f"), "./a/two.log\n");
    assert_eq!(run_find("find . \\( -name 'one*' -o -name 'two*' \\) -type f | wc -l").trim(), "2");
    assert_eq!(run_find("find . -path './b/*'"), "./b/three.txt\n");
    assert_eq!(run_find("find . -size +0c -type f"), "./a/one.txt\n");
    assert_eq!(run_find("find . -empty -type f | wc -l").trim(), "2");
    assert_eq!(run_find("find . -name one.txt -exec cat {} \\;"), "x\n");
    assert_eq!(run_find("find . -name '*.log' -delete; ls a"), "one.txt\n");
    assert_eq!(run_find("find a b -name '*.txt' -print"), "a/one.txt\nb/three.txt\n");
    assert_eq!(run_find("find . -name 'a' -prune -o -type f -print"), "./b/three.txt\n");
    assert_eq!(status("find /f -bogus"), 2);
    assert_eq!(status("find /nope"), 1);
}

#[test]
fn tar_and_gzip_round_trip() {
    assert_eq!(
        out("mkdir -p /t/s; echo hi > /t/s/f; echo yo > /t/g; cd /t; tar cf /a.tar s g; tar tf /a.tar"),
        "s/\ns/f\ng\n"
    );
    assert_eq!(
        out("mkdir -p /t/s; echo hi > /t/s/f; cd /t; tar -cf /a.tar s; mkdir /o; tar -xf /a.tar -C /o; cat /o/s/f"),
        "hi\n"
    );
    assert_eq!(
        out("mkdir /t; echo hi > /t/f; tar czf /a.tgz t; mkdir /o; cd /o; tar xzf /a.tgz; cat t/f; file -b /a.tgz"),
        "hi\ngzip compressed data\n"
    );
    assert_eq!(out("echo hi > /f; tar cf - f | tar tf -"), "f\n");
    assert_eq!(
        out("mkdir /t; touch /t/a /t/b; tar cf /x.tar t; mkdir /o; tar xf /x.tar -C /o t/b; ls /o/t"),
        "b\n"
    );
    assert_eq!(status("tar"), 2);
    assert_eq!(status("tar tf /missing"), 2);
    assert_eq!(status("echo not a tar > /b; tar tf /b"), 2);

    assert_eq!(out("echo hello > /f; gzip /f; ls /f* ; gunzip /f.gz; cat /f"), "/f.gz\nhello\n");
    assert_eq!(out("echo hello | gzip | gunzip"), "hello\n");
    assert_eq!(out("echo hello | gzip -c | gzip -d"), "hello\n");
    assert_eq!(out("echo hello > /f; gzip -k /f; zcat /f.gz; cat /f"), "hello\nhello\n");
    assert_eq!(out("echo hello > /f; gzip -c /f | zcat"), "hello\n");
    assert_eq!(status("echo x > /f; gunzip /f"), 1);
    assert_eq!(status("echo x > /f.gz; gunzip /f.gz"), 1);
    assert_eq!(out("seq 1 2000 > /big; gzip -k /big; gunzip -c /big.gz | tail -n 1"), "2000\n");
}

#[test]
fn tar_refuses_to_climb_out_of_the_target() {
    // A member named with `..` must not land outside the extraction directory.
    let mut shell = Shell::new(None, &NoSystem);
    let mut archive = alloc::vec![0u8; 2048];
    let length = {
        let mut writer = feos_archive_writer(&mut archive);
        writer.write_file("../evil", b"x").unwrap();
        writer.finish().unwrap()
    };
    archive.truncate(length);
    shell.fs.write_file("/a.tar", &archive, false).unwrap();
    let (code, _, err) = run_in(&mut shell, &["mkdir /o; tar xf /a.tar -C /o"], b"");
    assert_ne!(code, 0);
    assert!(err.contains(".."), "{err}");
    assert!(!shell.fs.exists("/evil"));
}

fn feos_archive_writer(buffer: &mut [u8]) -> feos_archive::tar::TarWriter<'_> {
    feos_archive::tar::TarWriter::new(buffer)
}

#[test]
fn sed_works_from_the_shell() {
    assert_eq!(out("echo hello | sed 's/l/L/g'"), "heLLo\n");
    assert_eq!(out("printf 'a\\nb\\n' | sed -n 2p"), "b\n");
    assert_eq!(out("echo x > /f; sed -i 's/x/y/' /f; cat /f"), "y\n");
    assert_eq!(out("echo abc | sed -E 's/(a)(b)/\\2\\1/'"), "bac\n");
    assert_eq!(out("printf 'a\\nb\\n' | sed -e 1d -e 's/b/c/'"), "c\n");
    assert_eq!(out("echo 's/a/b/' > /s; echo a | sed -f /s"), "b\n");
    assert_eq!(status("sed 's/'"), 1);
    assert_eq!(status("sed"), 2);
}

// ----- docs/terminal.md -----

/// Pulls the examples out of the documentation, so they cannot go stale.
fn documented_examples() -> Vec<String> {
    let doc = include_str!("../../../docs/terminal.md");
    let start = doc.find("```bash").expect("the examples block") + "```bash".len();
    let body = &doc[start..];
    let end = body.find("```").expect("the end of the examples");
    let mut examples = Vec::new();
    let mut current = String::new();
    for line in body[..end].lines() {
        if line.trim().is_empty() {
            if !current.trim().is_empty() {
                examples.push(core::mem::take(&mut current));
            }
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        examples.push(current);
    }
    examples
}

fn shell_with_documented_files<'a>() -> Shell<'a> {
    let mut shell = Shell::new(None, &NoSystem);
    let (code, _, err) = run_in(
        &mut shell,
        &["mkdir -p /docs /media /examples; \
           printf 'Scratch notes\\n' > /examples/notes.txt; \
           touch /docs/filesystem.md /docs/getting-started.md; \
           echo 12345 > /media/big; echo 1 > /media/small"],
        b"",
    );
    assert_eq!((code, err.as_str()), (0, ""));
    shell
}

#[test]
fn documented_examples_run_without_errors() {
    let examples = documented_examples();
    assert!(examples.len() >= 4, "found {examples:?}");
    for example in &examples {
        let mut shell = shell_with_documented_files();
        let (_, _, err) = run_in(&mut shell, &[example], b"");
        // `source` of the notes file runs prose as commands, which is what the
        // documentation shows; nothing else may complain.
        if !example.contains("notes.txt") {
            assert_eq!(err, "", "example failed:\n{example}");
        }
    }
}

#[test]
fn documented_examples_print_what_they_promise() {
    let mut shell = shell_with_documented_files();
    let (_, output, _) = run_in(&mut shell, &["ls /docs | wc -l"], b"");
    assert_eq!(output.trim(), "2");
    let (_, output, _) = run_in(&mut shell, &["ls -l /media | sort -k 5 -n | tail -n 1"], b"");
    assert!(output.contains("big"), "{output}");
    let (_, output, _) = run_in(
        &mut shell,
        &["printf 'pear\\napple\\napple\\n' | sort -u > /tmp/fruit", "cat /tmp/fruit | grep apple"],
        b"",
    );
    assert_eq!(output, "apple\n");
    let (_, output, _) = run_in(
        &mut shell,
        &["for n in 1 2 3 4 5; do\n  if test $((n % 2)) -eq 0; then\n    echo \"$n is even\"\n  fi\ndone"],
        b"",
    );
    assert_eq!(output, "2 is even\n4 is even\n");
    let (_, output, _) = run_in(&mut shell, &["echo \"today: $(uname -s) $(uname -m)\""], b"");
    assert_eq!(output, "today: FEOS unknown\n");
}

// ----- Robustness -----

#[test]
fn every_command_survives_empty_and_odd_arguments() {
    let odd: &[&[&str]] = &[
        &[],
        &[""],
        &["-"],
        &["--"],
        &["-n"],
        &["--nope"],
        &["\u{e9}"],
        &["/missing"],
        &["-1"],
        &["99999999999999999999"],
    ];
    for spec in feos_coreutils::COMMANDS {
        // `yes` is capped, and these are all quick.
        for args in odd {
            let mut shell = Shell::new(None, &NoSystem);
            let mut line = String::from(spec.name);
            if spec.name == "[" {
                line = String::from("\\[");
            }
            for arg in *args {
                line.push_str(" '");
                line.push_str(arg);
                line.push('\'');
            }
            line.push_str(" </dev/null");
            let mut input = BytesInput::new(b"some\ninput \xff\n".to_vec());
            let mut stdout = BytesOutput::new();
            let mut stderr = BytesOutput::new();
            let mut io = Io {
                stdin: &mut input,
                stdout: &mut stdout,
                stderr: &mut stderr,
            };
            let status = shell.execute_line(&line, &mut io, &mut NoHost);
            // `env` and `xargs` run their argument as a command.
            if !matches!(spec.name, "env" | "xargs") {
                assert_ne!(status, 127, "{line}");
            }
        }
    }
}

#[test]
fn syntax_fuzz_never_panics() {
    let fragments = [
        "echo", "$", "${", "}", "$(", ")", "(", "((", "))", "'", "\"", "`", "\\", "<<", "<<<", "<",
        ">", ">>", "&", "&&", "|", "||", ";", ";;", "\n", "if", "then", "fi", "for", "in", "do",
        "done", "case", "esac", "while", "[[", "]]", "!", "{", "}", "f()", "x=", "$x", "$@", "$((",
        "${x:-", "${#", "${x%", "*", "?", "[", "]", "~", "#", "=", "2>&1", "é", "$'", "\\n",
    ];
    // A small deterministic generator walks a lot of combinations.
    let mut state = 0x1234_5678_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as usize
    };
    for _ in 0..4000 {
        let count = next() % 8 + 1;
        let mut line = String::new();
        for _ in 0..count {
            line.push_str(fragments[next() % fragments.len()]);
            if next() % 2 == 0 {
                line.push(' ');
            }
        }
        let mut shell = Shell::new(None, &NoSystem);
        let mut host = CancellingHost { polls_left: 2000 };
        let mut input = BytesInput::empty();
        let mut stdout = BytesOutput::new();
        let mut stderr = BytesOutput::new();
        let mut io = Io {
            stdin: &mut input,
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        shell.execute_line(&line, &mut io, &mut host);
        let _ = crate::lexer::needs_more(&line);
        let _ = shell.complete(&line, line.len() / 2, &NoHost);
        let _ = line.to_string();
    }
}

#[test]
fn shell_state_is_send_for_the_terminal_static() {
    fn assert_send<T: Send>() {}
    assert_send::<Shell<'static>>();
}
