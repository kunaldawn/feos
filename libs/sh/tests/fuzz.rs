//! Shell fuzzing under the kernel's constraints: a 64 MiB heap and a kernel-sized
//! stack, from the parsers' shared harness. Going past either aborts or faults
//! the binary, so out-of-memory and runaway recursion count as failures.
//!
//! The last test also refuses every large request, so the shell's byte budgets
//! take their `try_reserve` failure paths. A large buffer taken without one
//! aborts the binary there, as it would take the kernel's heap.

#[path = "../../compress/tests/support/fuzz.rs"]
mod fuzz;

use feos_coreutils::{BytesInput, BytesOutput, Input, NoSystem, Output, COMMANDS};
use feos_sh::{Host, Io, Shell};
use fuzz::{
    refused, run_constrained, scaled, stack_limit, with_request_limit, LimitAllocator, Rng, KERNEL_STACK,
};

#[global_allocator]
static ALLOCATOR: LimitAllocator = LimitAllocator;

/// Runs nothing external, and cancels a line once it has polled too often, so
/// that `while :` and `yes` end.
struct FuzzHost {
    polls_left: u32,
}

impl Host for FuzzHost {
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

/// A shell whose nesting check fits the harness's stack. On a kernel-sized stack,
/// as `make fuzz` gives, that's the release build's limit: the `fuzz` profile is
/// optimised, but its debug assertions would pick the larger debug one.
fn new_shell() -> Shell<'static> {
    let mut shell = Shell::new(None, &NoSystem);
    if stack_limit() <= KERNEL_STACK {
        shell.set_stack_limit(88 * 1024);
    }
    shell
}

fn run(shell: &mut Shell<'_>, line: &str, stdin: &[u8]) -> (i32, Vec<u8>) {
    run_polling(shell, line, stdin, 2000)
}

fn run_polling(shell: &mut Shell<'_>, line: &str, stdin: &[u8], polls: u32) -> (i32, Vec<u8>) {
    let mut input = BytesInput::new(stdin.to_vec());
    let mut stdout = BytesOutput::new();
    let mut stderr = BytesOutput::new();
    let mut io = Io {
        stdin: &mut input,
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    let status = shell.execute_line(line, &mut io, &mut FuzzHost { polls_left: polls });
    (status, stdout.bytes().to_vec())
}

#[test]
fn syntax_fuzz_under_the_heap_limit() {
    run_constrained(|| {
        let fragments = [
            "echo", "$", "${", "}", "$(", ")", "(", "((", "))", "'", "\"", "`", "\\", "<<", "<<<",
            "<", ">", ">>", "&", "&&", "|", "||", ";", ";;", "\n", "if", "then", "fi", "for", "in",
            "do", "done", "case", "esac", "while", "[[", "]]", "!", "{", "}", "f()", "x=", "$x",
            "$@", "$((", "${x:-", "${#", "${x%", "*", "?", "[", "]", "~", "#", "=", "2>&1", "é",
            "$'", "\\n", "f", "$(f)", "eval", "x=$x$x", "seq 9999", "|sort",
        ];
        let mut rng = Rng::new(0x5348_4655_5a5a);
        for _ in 0..scaled(3000) {
            let mut line = String::new();
            for _ in 0..1 + rng.below(10) {
                line.push_str(fragments[rng.below(fragments.len())]);
                if rng.below(2) == 0 {
                    line.push(' ');
                }
            }
            let mut shell = new_shell();
            run(&mut shell, &line, b"");
            let _ = feos_sh::lexer::needs_more(&line);
            let _ = shell.complete(&line, line.len() / 2, &FuzzHost { polls_left: 1 });
        }
    });
}

#[test]
fn command_fuzz_under_the_heap_limit() {
    run_constrained(|| {
        let args = [
            "", "-", "--", "-n", "-1", "0", "3", "99999999999999999999", "-e", "-v", "-r", "-k",
            "2", "-t", ":", "-s", "a", "x*", "/", "/f", "/missing", "/f/x", ".", "..", "s/a/b/g",
            "{print $1}", "%s %d", "é", "\\", "'", "[a-z]+", "-c", "-w", "-l", "+1", "-z",
        ];
        let inputs: [&[u8]; 4] = [b"", b"a b c\n1 2 3\n", b"\xff\xfe\x00\n\n", b"x\ty\tz\r\n"];
        let mut rng = Rng::new(0x434d_4446_555a);
        for spec in COMMANDS {
            if matches!(spec.name, "env" | "xargs" | "sh" | "watch" | "time" | "timeout") {
                // These run their argument as a command, which the syntax fuzz covers.
                continue;
            }
            for _ in 0..scaled(12) {
                let mut line = if spec.name == "[" {
                    String::from("\\[")
                } else {
                    String::from(spec.name)
                };
                for _ in 0..rng.below(5) {
                    line.push_str(" '");
                    line.push_str(&args[rng.below(args.len())].replace('\'', ""));
                    line.push('\'');
                }
                let mut shell = new_shell();
                run(&mut shell, "printf 'one\\ntwo\\n' > /f", b"");
                run(&mut shell, &line, inputs[rng.below(inputs.len())]);
            }
        }
    });
}

#[test]
fn refused_large_requests_end_the_command_not_the_shell() {
    run_constrained(|| {
        let mut shell = new_shell();
        // About 2 MiB, made before the limit so the lines below have it to read.
        let (status, _) = run_polling(&mut shell, "seq 300000 > /big", b"", u32::MAX);
        assert_eq!(status, 0);
        let before = refused();
        with_request_limit(256 * 1024, || {
            for line in [
                "cat /big | wc -c",
                "x=$(cat /big); echo ${#x}",
                "sort /big | tail -n 1",
                "cat /big /big > /bigger",
                "tr 0-9 a-j < /big | wc -l",
                "grep -c 9 /big",
                "seq 400000 | wc -l",
            ] {
                run_polling(&mut shell, line, b"", u32::MAX);
                let (status, output) = run(&mut shell, "echo still here", b"");
                assert_eq!((status, output.as_slice()), (0, &b"still here\n"[..]), "after {line}");
            }
        });
        assert!(refused() > before, "nothing asked for a large buffer");
    });
}
