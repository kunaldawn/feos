//! Runs the same scripts on the host's shell and on `feos-sh`, comparing standard
//! output and exit status. The host shell is the reference for language
//! behaviour. Standard error is not compared, because message wording differs.
//! When the host has no `bash` or `dash`, the tests pass without checking.

use std::process::Command;

use feos_coreutils::{BytesInput, BytesOutput, Input, NoSystem, Output};
use feos_sh::{Host, Io, Shell};

struct NoHost;

impl Host for NoHost {
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
}

fn run_feos(script: &str) -> (i32, String) {
    let mut shell = Shell::new(None, &NoSystem);
    let mut stdin = BytesInput::empty();
    let mut stdout = BytesOutput::new();
    let mut stderr = BytesOutput::new();
    let mut io = Io {
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    // A script run by `sh -c`, so it behaves as non-interactive on both sides.
    let wrapped = format!("mkdir /work; cd /work; sh -c {}", quote(script));
    let status = shell.execute_line(&wrapped, &mut io, &mut NoHost);
    (status, String::from_utf8_lossy(stdout.bytes()).into_owned())
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn run_host(shell: &str, script: &str, directory: &std::path::Path) -> Option<(i32, String)> {
    let output = Command::new(shell)
        .arg("-c")
        .arg(script)
        .current_dir(directory)
        .env("LC_ALL", "C")
        .env("HOME", "/")
        .env_remove("IFS")
        .env_remove("POSIXLY_CORRECT")
        .output()
        .ok()?;
    Some((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

fn available(shell: &str) -> bool {
    Command::new(shell)
        .arg("-c")
        .arg("exit 0")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn compare(shell: &str, scripts: &[&str]) {
    let mut failures = Vec::new();
    for (index, script) in scripts.iter().enumerate() {
        let directory = std::env::temp_dir().join(format!(
            "feos-diff-{}-{shell}-{index}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let expected = run_host(shell, script, &directory);
        let _ = std::fs::remove_dir_all(&directory);
        let Some(expected) = expected else {
            continue;
        };
        let actual = run_feos(script);
        if expected != actual {
            failures.push(format!(
                "script: {script}\n  {shell}: {expected:?}\n  feos: {actual:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} scripts differ:\n{}",
        failures.len(),
        scripts.len(),
        failures.join("\n")
    );
}

/// What both `dash` and `bash` agree on.
const POSIX: &[&str] = &[
    "echo hello",
    "echo a b   c",
    "echo 'a  b' \"c  d\" e\\ f",
    "x=1; echo $x ${x} \"$x\"",
    "x='a b'; y=$x; echo \"[$y]\"",
    "x='a  b'; echo $x; echo \"$x\"",
    "echo $UNSET_VAR.",
    "false; echo $?; true; echo $?",
    "exit 3",
    "exit 300",
    "(exit 4); echo $?",
    "x=$(false); echo $?",
    "x=$(echo a; exit 5); echo $x $?",
    "echo $(echo a b) \"$(echo c  d)\"",
    "echo `echo back`",
    "echo $(echo $(echo nested))",
    "echo $(printf 'a\\n\\n\\n')x",
    "echo $((1 + 2 * 3)) $(((1 + 2) * 3)) $((7 / 2)) $((7 % 3))",
    "n=5; echo $((n + 1)) $(($n + 1)) $((n * n))",
    "echo $((3 > 5)) $((3 < 5)) $((1 == 1)) $((1 != 1))",
    "echo $((1 && 0)) $((1 || 0)) $((!0)) $((~0))",
    "echo $((1 ? 2 : 3)) $((0 ? 2 : 3))",
    "echo $((0x10)) $((010)) $((1 << 3)) $((16 >> 2)) $((6 & 3)) $((6 | 3)) $((6 ^ 3))",
    "x=1; : $((x += 2)); echo $x",
    "x=5; : $((x = x * 2)); echo $x",
    "if true; then echo a; else echo b; fi",
    "if false; then echo a; elif true; then echo b; else echo c; fi",
    "if false; then echo a; fi; echo $?",
    "for x in a b c; do echo $x; done",
    "for x in 'a b' c; do echo \"$x\"; done",
    "set -- p q; for x; do echo $x; done",
    "set -- p q; for x in \"$@\"; do echo $x; done",
    "for x in; do echo no; done; echo $?",
    "i=0; while [ $i -lt 3 ]; do echo $i; i=$((i + 1)); done",
    "i=0; until [ $i -ge 2 ]; do echo $i; i=$((i + 1)); done",
    "for i in 1 2 3; do if [ $i = 2 ]; then break; fi; echo $i; done",
    "for i in 1 2 3; do if [ $i = 2 ]; then continue; fi; echo $i; done",
    "for i in 1 2; do for j in a b; do echo $i$j; break 2; done; done",
    "for i in 1 2; do for j in a b; do echo $i$j; continue 2; done; done",
    "case abc in a*) echo y;; *) echo n;; esac",
    "case x in a|x) echo one;; esac",
    "case x in (x) echo paren;; esac",
    "case 'a b' in 'a b') echo q;; esac",
    "case '*' in \\*) echo star;; esac",
    "x=hello; case $x in h*o) echo match;; esac",
    "case a in b) echo no;; esac; echo $?",
    "f() { echo $1 $#; }; f a b",
    "f() { return 7; }; f; echo $?",
    "f() { x=in; }; x=out; f; echo $x",
    "f() { echo \"$@\"; shift; echo \"$@\"; }; f 1 2 3",
    "fact() { if [ $1 -le 1 ]; then echo 1; else echo $(($1 * $(fact $(($1 - 1))))); fi; }; fact 5",
    "f() { for i in 1 2; do return 3; done; }; f; echo $?",
    "set -- a b c; echo $# $1 $3; shift; echo $# $1; shift 2; echo $#",
    "set -- 'a b' c; printf '<%s>' \"$@\"; echo; printf '<%s>' $@; echo; printf '<%s>' \"$*\"; echo",
    "set --; printf '<%s>' \"$@\"; echo",
    "set -- a; printf '<%s>' \"x$@y\"; echo",
    "set -- a b; printf '<%s>' \"x$@y\"; echo",
    "echo ${X:-d} ${X-e}; X=; echo ${X:-d} ${X-e}.; X=v; echo ${X:-d} ${X-e}",
    "echo ${U:=v} $U",
    "X=s; echo ${X:+alt} ${Y:+alt}.",
    "x=/a/b.tar.gz; echo ${x##*/} ${x%%.*} ${x%.*} ${x#*/}",
    "x=hello; echo ${#x}",
    "x='a b'; printf '<%s>' ${U:-$x} \"${U:-$x}\"; echo",
    "foo=a; echo ${X:-$foo} ${X:-$(echo c)} ${X:-${Y:-deep}}",
    "x=abc; echo ${x%c}d ${x#a}",
    "echo ${X:-ok}; echo ${Y:?msg}; echo no",
    "IFS=,; x=a,b,,c; printf '<%s>' $x; echo",
    "IFS=,; x=,a,; printf '<%s>' $x; echo",
    "IFS=; x='a b'; printf '<%s>' $x; echo",
    "IFS=' ,'; x='a , b'; printf '<%s>' $x; echo",
    "IFS=:; set -- a b; echo \"$*\"",
    "x=; printf '<%s>' a$x b \"$x\"; echo",
    "IFS=: read a b <<EOF\nx:y z\nEOF\necho \"$a|$b\"",
    "read a b <<EOF\n  1 2  3  \nEOF\necho \"[$a|$b]\"",
    "read a <<EOF\none\\\ntwo\nEOF\necho \"$a\"",
    "cat <<EOF\nplain\nEOF",
    "x=1; cat <<EOF\nv=$x $((1 + 1)) $(echo s) \\$x\nEOF",
    "x=1; cat <<'EOF'\n$x \\n\nEOF",
    "cat <<-E\n\ta\n\tE",
    "cat <<A; cat <<B\nfirst\nA\nsecond\nB",
    "while read l; do echo \"<$l>\"; done <<E\n1\n2\nE",
    "echo hi > f; cat f; echo more >> f; cat f",
    "echo hi > f; cat < f",
    "cat missing 2>/dev/null; echo $?",
    "cat missing > o 2>&1; wc -l < o",
    "cat missing 2>&1 > o | wc -l; wc -c < o",
    "echo a > f; { read x; echo got $x; } < f",
    "for i in 1 2; do echo $i; done > f; cat f",
    "{ echo a; echo b; } > f; cat f",
    "(echo s) > f; cat f",
    "if true; then echo y; fi > f; cat f",
    "exec 3> f; echo x >&3; exec 3>&-; cat f",
    "echo hi >&2 2>/dev/null",
    "echo x > /nonexistent/file; echo $?",
    "echo a | cat; echo $?",
    "false | true; echo $?",
    "true | false; echo $?",
    "! true; echo $?; ! false; echo $?",
    "echo a | { read x; echo $x; }",
    "x=1; echo a | { read x; }; echo $x",
    "true && echo a; false && echo b; false || echo c; true || echo d",
    "true && false || echo e",
    "x=1; (x=2; echo $x); echo $x",
    "mkdir d; (cd d; basename \"$PWD\"); basename \"$PWD\" >/dev/null; echo ok",
    "echo $(x=2; echo $x); echo \"[$x]\"",
    "(exit 3) || echo failed",
    "sh -c 'exit 3'; echo $?",
    "sh -c 'echo $0 $1' zero one",
    "x=1; sh -c 'echo \"[$x]\"'; export x; sh -c 'echo \"[$x]\"'",
    "FOO=0; FOO=1 echo $FOO",
    "FOO=1 sh -c 'echo $FOO'; echo \"[$FOO]\"",
    "unset x; echo \"[${x-unset}]\"; x=; echo \"[${x-unset}]\"",
    "set -e; false; echo no",
    "set -e; false || true; echo yes",
    "set -e; if false; then :; fi; echo yes",
    "set -e; ! false; echo yes",
    "set -e; true && false; echo no",
    "set -e; (false); echo no",
    "set -u; echo $NOPE; echo no",
    "set -u; echo ${NOPE:-ok}",
    "set -f; touch a; echo *",
    "touch b a c; echo *; echo [ab]; echo ?; echo [!a]",
    "touch fooa foob; rm foo\\*; ls fooa foob 2>/dev/null | wc -l",
    "touch fooa; echo foo\"*\" 'foo*' foo*",
    "mkdir d; touch f; echo */",
    "touch .h v; echo *; echo .h*",
    "echo nomatch*",
    "readonly R=1; echo $R; R=2; echo no",
    "readonly R=1; unset R; echo $?",
    "while getopts ab: o -a -b v x; do echo $o $OPTARG; done; echo $OPTIND",
    "set -- -a -b v rest; while getopts ab: o; do echo $o; done; shift $((OPTIND - 1)); echo $*",
    "getopts :a o -z; echo \"$o $OPTARG\"",
    "printf '%s|%d|%x|%5s|%-5s|\\n' a 10 255 b c",
    "printf '%s\\n' a b c",
    "printf '%03d %+d %c\\n' 5 5 hello",
    "test 1 -eq 1; echo $?; test 1 -eq 2; echo $?; test a = a; echo $?; test a != a; echo $?",
    "[ -n a ]; echo $?; [ -z a ]; echo $?; [ -e /nope ]; echo $?; [ -d / ]; echo $?",
    "[ a ]; echo $?; [ '' ]; echo $?; [ ]; echo $?; [ ! a ]; echo $?",
    "[ 1 -lt 2 -a 2 -lt 3 ]; echo $?; [ 1 -gt 2 -o 2 -lt 3 ]; echo $?",
    "eval 'echo $((1 + 1))'; x='echo y'; eval \"$x\"",
    "echo 'echo sourced $1' > s; . ./s arg; set -- own; . ./s",
    "echo 'return 4; echo no' > s; . ./s; echo $?",
    "printf 'echo script $1\\n' > s; sh s a; sh ./s b",
    "echo 'echo piped' | sh",
    "seq 1 3 | while read n; do echo $((n * 2)); done",
    "printf 'b\\na\\nb\\n' | sort | uniq -c | wc -l",
    "printf 'a,b\\n' | cut -d, -f2",
    "echo hello | tr a-y b-z",
    "printf 'x\\ny\\n' | head -n 1; printf 'x\\ny\\n' | tail -n 1",
    "printf 'one\\ntwo\\n' | grep -c o",
    "printf 'one\\ntwo\\n' | grep -v one",
    "printf 'one\\ntwo\\n' | sed 's/o/0/g'",
    "expr 2 + 3 '*' 4; expr 7 / 2; expr abc : 'a.'",
    "echo a\\\nb",
    "echo #comment\necho a#b",
    "x=1 y=2; echo $x$y",
    "{ echo in; }; echo out",
    "echo a; echo b; echo c",
    "if true\nthen\n  echo multi\nfi",
    "for i in 1 2\ndo\n  echo $i\ndone",
    "f()\n{\n  echo body\n}\nf",
    "case a in\n  a)\n    echo nl\n    ;;\nesac",
    "echo a &&\necho b",
    "echo a |\ncat",
];

/// What only `bash` accepts.
const BASH_ONLY: &[&str] = &[
    "x=a; x+=b; echo $x",
    "echo $'a\\tb' $'it\\'s' $'\\x41'",
    "x=abcabc; echo ${x/b/X} ${x//b/X} ${x/#a/X} ${x/%c/X}",
    "x=hello; echo ${x:1:3} ${x: -2} ${x:2}",
    "x=hello; echo ${x^} ${x^^}; y=HI; echo ${y,} ${y,,}",
    "x=y; y=z; echo ${!x}",
    "set -- a b c d; echo ${@:2:2} ${#@}",
    "(( 1 < 2 )); echo $?; (( 0 )); echo $?",
    "((x = 3 + 4)); echo $x; ((x++)); echo $x",
    "let 'x=2*3' y=x+1; echo $x $y",
    "for ((i = 0; i < 2; i++)); do echo $i; done",
    "echo $((2 ** 10))",
    "[[ abc == a* ]]; echo $?; [[ abc == 'a*' ]]; echo $?; [[ abc != a* ]]; echo $?",
    "x='a b'; [[ $x == 'a b' ]]; echo $?",
    "[[ -z '' && -n a ]]; echo $?; [[ -f /nope || a == a ]]; echo $?",
    "[[ abc =~ ^a(b|x)c$ ]]; echo $?; [[ abc =~ z ]]; echo $?",
    "[[ a < b ]]; echo $?; [[ 2 -lt 10 ]]; echo $?",
    "function f { echo g; }; f",
    "function f() { echo h; }; f",
    "f() { local x=in; echo $x; }; x=out; f; echo $x",
    "f() { local x; echo \"[$x]\"; x=1; }; x=out; f; echo $x",
    "f() { local x=1; g; echo $x; }; g() { x=2; }; f",
    "cat <<< here; x='a b'; cat <<< \"$x\"",
    "read a b <<< 'one two'; echo $b $a",
    "echo hi &> f; cat f; echo more &>> f; cat f",
    "ls /nonexistent |& wc -l",
    "set -o pipefail; false | true; echo $?; set +o pipefail; false | true; echo $?",
    "case a in a) echo 1;& b) echo 2;; c) echo 3;; esac",
    "case a in a) echo 1;;& a) echo 2;; *) echo 3;; esac",
    "set -C; echo a > k; echo b > k; echo $?; echo c >| k; cat k",
    "exec 3<<< data; read x <&3; echo $x",
    "declare x=1; echo $x",
    "x=$(cat <<E\nsub\nE\n); echo $x",
];

#[test]
fn matches_the_host_shell_on_posix_scripts() {
    let shell = if available("bash") {
        "bash"
    } else if available("dash") {
        "dash"
    } else {
        eprintln!("skipping: the host has neither bash nor dash");
        return;
    };
    compare(shell, POSIX);
}

#[test]
fn matches_bash_on_bash_scripts() {
    if !available("bash") {
        eprintln!("skipping: the host has no bash");
        return;
    }
    compare("bash", BASH_ONLY);
}
