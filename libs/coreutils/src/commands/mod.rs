//! Command registry and dispatch for [`crate::run`].

mod files;
mod system;
mod text;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::env::Env;
use crate::fs::{FsError, Vfs};
use crate::io::{Input, Output};
use crate::system::System;

/// A command name, its help text and aliases.
#[derive(Clone, Copy, Debug)]
pub struct CommandSpec {
    /// Primary command name.
    pub name: &'static str,
    /// Alternative names.
    pub aliases: &'static [&'static str],
    /// Short usage line.
    pub usage: &'static str,
    /// One-line description.
    pub description: &'static str,
}

/// Everything a command may touch.
pub struct Ctx<'a> {
    /// Writable filesystem view.
    pub fs: &'a mut dyn Vfs,
    /// Shell/environment variables.
    pub env: &'a mut Env,
    /// Standard input.
    pub stdin: &'a mut dyn Input,
    /// Standard output.
    pub stdout: &'a mut dyn Output,
    /// Standard error.
    pub stderr: &'a mut dyn Output,
    /// Platform facts.
    pub system: &'a dyn System,
}

/// Every command implemented by this crate, in help order.
pub static COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "ls",
        aliases: &["dir"],
        usage: "ls [-la1hRFr] [path...]",
        description: "list directory contents",
    },
    CommandSpec {
        name: "cat",
        aliases: &[],
        usage: "cat [-nbsA] [file...]",
        description: "concatenate files",
    },
    CommandSpec {
        name: "echo",
        aliases: &[],
        usage: "echo [-neE] [text...]",
        description: "print text",
    },
    CommandSpec {
        name: "printf",
        aliases: &[],
        usage: "printf FORMAT [arg...]",
        description: "format and print data",
    },
    CommandSpec {
        name: "pwd",
        aliases: &[],
        usage: "pwd",
        description: "print the working directory",
    },
    CommandSpec {
        name: "mkdir",
        aliases: &[],
        usage: "mkdir [-pv] DIR...",
        description: "create directories",
    },
    CommandSpec {
        name: "rmdir",
        aliases: &[],
        usage: "rmdir [-pv] DIR...",
        description: "remove empty directories",
    },
    CommandSpec {
        name: "rm",
        aliases: &[],
        usage: "rm [-rfdv] PATH...",
        description: "remove files and directories",
    },
    CommandSpec {
        name: "cp",
        aliases: &[],
        usage: "cp [-rRfvni] SRC... DEST",
        description: "copy files and directories",
    },
    CommandSpec {
        name: "mv",
        aliases: &[],
        usage: "mv [-fvn] SRC... DEST",
        description: "move or rename files",
    },
    CommandSpec {
        name: "touch",
        aliases: &[],
        usage: "touch [-c] FILE...",
        description: "create empty files",
    },
    CommandSpec {
        name: "head",
        aliases: &[],
        usage: "head [-n N|-c N] [file...]",
        description: "first lines of files",
    },
    CommandSpec {
        name: "tail",
        aliases: &[],
        usage: "tail [-n N|-c N] [file...]",
        description: "last lines of files",
    },
    CommandSpec {
        name: "wc",
        aliases: &[],
        usage: "wc [-lwcm] [file...]",
        description: "count lines, words and bytes",
    },
    CommandSpec {
        name: "grep",
        aliases: &[],
        usage: "grep [-ivnclLqwxFr] PATTERN [file...]",
        description: "search text with a pattern",
    },
    CommandSpec {
        name: "sort",
        aliases: &[],
        usage: "sort [-rnufb] [-t C] [-k N] [file...]",
        description: "sort lines",
    },
    CommandSpec {
        name: "uniq",
        aliases: &[],
        usage: "uniq [-cdui] [file]",
        description: "filter repeated lines",
    },
    CommandSpec {
        name: "cut",
        aliases: &[],
        usage: "cut [-d C] [-f LIST] [-c LIST] [file...]",
        description: "select fields or columns",
    },
    CommandSpec {
        name: "tr",
        aliases: &[],
        usage: "tr [-dsc] SET1 [SET2]",
        description: "translate or delete bytes",
    },
    CommandSpec {
        name: "seq",
        aliases: &[],
        usage: "seq [-w] [-s SEP] [first [step]] last",
        description: "print a sequence of numbers",
    },
    CommandSpec {
        name: "basename",
        aliases: &[],
        usage: "basename PATH [SUFFIX]",
        description: "strip the directory from a path",
    },
    CommandSpec {
        name: "dirname",
        aliases: &[],
        usage: "dirname PATH",
        description: "strip the last path component",
    },
    CommandSpec {
        name: "nl",
        aliases: &[],
        usage: "nl [-b a|t] [-w N] [file...]",
        description: "number lines",
    },
    CommandSpec {
        name: "rev",
        aliases: &[],
        usage: "rev [file...]",
        description: "reverse each line",
    },
    CommandSpec {
        name: "tac",
        aliases: &[],
        usage: "tac [file...]",
        description: "print lines in reverse order",
    },
    CommandSpec {
        name: "paste",
        aliases: &[],
        usage: "paste [-d LIST] [-s] [file...]",
        description: "merge lines of files",
    },
    CommandSpec {
        name: "tee",
        aliases: &[],
        usage: "tee [-a] [file...]",
        description: "copy input to files and stdout",
    },
    CommandSpec {
        name: "cmp",
        aliases: &[],
        usage: "cmp [-sl] FILE1 FILE2",
        description: "compare two files byte by byte",
    },
    CommandSpec {
        name: "find",
        aliases: &[],
        usage: "find [path...] [-name PAT] [-type f|d] [-maxdepth N]",
        description: "walk a directory tree",
    },
    CommandSpec {
        name: "stat",
        aliases: &[],
        usage: "stat PATH...",
        description: "show file metadata",
    },
    CommandSpec {
        name: "tree",
        aliases: &[],
        usage: "tree [-a] [-L N] [path]",
        description: "display a directory tree",
    },
    CommandSpec {
        name: "du",
        aliases: &[],
        usage: "du [-sah] [path...]",
        description: "estimate file space usage",
    },
    CommandSpec {
        name: "df",
        aliases: &[],
        usage: "df [-h]",
        description: "show filesystem space",
    },
    CommandSpec {
        name: "env",
        aliases: &[],
        usage: "env",
        description: "print the environment",
    },
    CommandSpec {
        name: "printenv",
        aliases: &[],
        usage: "printenv [NAME...]",
        description: "print environment variables",
    },
    CommandSpec {
        name: "uname",
        aliases: &[],
        usage: "uname [-asnrvm]",
        description: "print system information",
    },
    CommandSpec {
        name: "whoami",
        aliases: &[],
        usage: "whoami",
        description: "print the current user",
    },
    CommandSpec {
        name: "id",
        aliases: &[],
        usage: "id",
        description: "print user and group ids",
    },
    CommandSpec {
        name: "hostname",
        aliases: &[],
        usage: "hostname",
        description: "print the host name",
    },
    CommandSpec {
        name: "nproc",
        aliases: &[],
        usage: "nproc",
        description: "print the number of CPUs",
    },
    CommandSpec {
        name: "arch",
        aliases: &[],
        usage: "arch",
        description: "print the machine architecture",
    },
    CommandSpec {
        name: "uptime",
        aliases: &[],
        usage: "uptime",
        description: "show how long the system has been up",
    },
    CommandSpec {
        name: "free",
        aliases: &[],
        usage: "free [-h]",
        description: "show memory usage",
    },
    CommandSpec {
        name: "hexdump",
        aliases: &["xxd"],
        usage: "hexdump [-C] [-n N] [file...]",
        description: "dump bytes as hexadecimal",
    },
    CommandSpec {
        name: "base64",
        aliases: &[],
        usage: "base64 [-d] [-w N] [file...]",
        description: "encode or decode base64",
    },
    CommandSpec {
        name: "sha256sum",
        aliases: &[],
        usage: "sha256sum [file...]",
        description: "print SHA-256 checksums",
    },
    CommandSpec {
        name: "test",
        aliases: &["["],
        usage: "test EXPRESSION",
        description: "evaluate a conditional expression",
    },
    CommandSpec {
        name: "expr",
        aliases: &[],
        usage: "expr ARG...",
        description: "evaluate an integer expression",
    },
    CommandSpec {
        name: "true",
        aliases: &[":"],
        usage: "true",
        description: "do nothing successfully",
    },
    CommandSpec {
        name: "false",
        aliases: &[],
        usage: "false",
        description: "do nothing unsuccessfully",
    },
    CommandSpec {
        name: "yes",
        aliases: &[],
        usage: "yes [text]",
        description: "repeat a line (capped for safety)",
    },
];

/// Finds a command by name or alias.
pub fn command(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
}

/// Returns `true` when `name` is a coreutils command.
pub fn is_command(name: &str) -> bool {
    command(name).is_some()
}

/// Runs the command `name` with `args`, returning its exit status.
pub fn run(name: &str, args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    match name {
        "ls" | "dir" => files::ls(args, ctx),
        "cat" => files::cat(args, ctx),
        "mkdir" => files::mkdir(args, ctx),
        "rmdir" => files::rmdir(args, ctx),
        "rm" => files::rm(args, ctx),
        "cp" => files::cp(args, ctx),
        "mv" => files::mv(args, ctx),
        "touch" => files::touch(args, ctx),
        "find" => files::find(args, ctx),
        "stat" => files::stat(args, ctx),
        "tree" => files::tree(args, ctx),
        "du" => files::du(args, ctx),
        "df" => files::df(args, ctx),
        "echo" => text::echo(args, ctx),
        "printf" => text::printf(args, ctx),
        "head" => text::head(args, ctx),
        "tail" => text::tail(args, ctx),
        "wc" => text::wc(args, ctx),
        "grep" => text::grep(args, ctx),
        "sort" => text::sort(args, ctx),
        "uniq" => text::uniq(args, ctx),
        "cut" => text::cut(args, ctx),
        "tr" => text::tr(args, ctx),
        "seq" => text::seq(args, ctx),
        "nl" => text::nl(args, ctx),
        "rev" => text::rev(args, ctx),
        "tac" => text::tac(args, ctx),
        "paste" => text::paste(args, ctx),
        "tee" => text::tee(args, ctx),
        "cmp" => text::cmp(args, ctx),
        "base64" => text::base64(args, ctx),
        "hexdump" | "xxd" => text::hexdump(args, ctx),
        "sha256sum" => text::sha256sum(args, ctx),
        "env" => system::env(args, ctx),
        "printenv" => system::printenv(args, ctx),
        "uname" => system::uname(args, ctx),
        "whoami" => system::whoami(args, ctx),
        "id" => system::id(args, ctx),
        "hostname" => system::hostname(args, ctx),
        "nproc" => system::nproc(args, ctx),
        "arch" => system::arch(args, ctx),
        "uptime" => system::uptime(args, ctx),
        "free" => system::free(args, ctx),
        "test" | "[" => system::test(args, ctx),
        "expr" => system::expr(args, ctx),
        "true" | ":" => 0,
        "false" => 1,
        "yes" => text::yes(args, ctx),
        _ => {
            error(ctx.stderr, name, "command not found");
            127
        }
    }
}

/// A [`fmt::Write`] adapter over an output sink.
pub(super) struct Out<'a>(pub &'a mut dyn Output);

impl Write for Out<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.write(text.as_bytes());
        Ok(())
    }
}

/// Writes formatted output without a newline.
pub(super) fn write_out(out: &mut dyn Output, args: fmt::Arguments<'_>) {
    let _ = write!(Out(out), "{args}");
}

/// Writes formatted output with a newline.
pub(super) fn write_line(out: &mut dyn Output, args: fmt::Arguments<'_>) {
    let _ = writeln!(Out(out), "{args}");
}

/// Prints `cmd: message` to standard error.
pub(super) fn error(out: &mut dyn Output, cmd: &str, message: &str) {
    let _ = writeln!(Out(out), "{cmd}: {message}");
}

/// Prints `cmd: path: strerror` to standard error.
pub(super) fn path_error(out: &mut dyn Output, cmd: &str, path: &str, err: FsError) {
    let _ = writeln!(Out(out), "{cmd}: {path}: {}", err.text());
}

/// Prints a usage error and returns status 2.
pub(super) fn usage(out: &mut dyn Output, cmd: &str, usage: &str) -> i32 {
    let _ = writeln!(Out(out), "{cmd}: usage: {usage}");
    2
}

/// Formats a byte count in human units.
pub(super) fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        alloc::format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        alloc::format!("{}K", (bytes + 512) / 1024)
    } else {
        alloc::format!("{}M", (bytes + 512 * 1024) / (1024 * 1024))
    }
}

/// Returns `true` when `arg` is a short/long option and not a bare `-`.
pub(super) fn is_option(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-')
}

/// Parses an integer option value.
pub(super) fn parse_usize(text: &str) -> Option<usize> {
    text.parse().ok()
}

/// Parses an integer, accepting a leading `+`.
pub(super) fn parse_i64(text: &str) -> Option<i64> {
    let text = text.strip_prefix('+').unwrap_or(text);
    text.parse().ok()
}

/// Reads all of a file, or standard input for `-`.
pub(super) fn read_path(ctx: &mut Ctx<'_>, path: &str) -> Result<Vec<u8>, FsError> {
    if path == "-" {
        Ok(ctx.stdin.read_to_end())
    } else {
        ctx.fs.read_file(path)
    }
}

/// Writes a file, reporting errors through `cmd`.
pub(super) fn write_path(
    ctx: &mut Ctx<'_>,
    cmd: &str,
    path: &str,
    data: &[u8],
    append: bool,
) -> bool {
    match ctx.fs.write_file(path, data, append) {
        Ok(()) => true,
        Err(err) => {
            path_error(ctx.stderr, cmd, path, err);
            false
        }
    }
}

/// Splits bytes into lines, keeping the trailing newline in each line.
pub(super) fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, byte) in data.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(&data[start..=index]);
            start = index + 1;
        }
    }
    if start < data.len() {
        lines.push(&data[start..]);
    }
    lines
}

/// Splits text into lines without their newline characters.
pub(super) fn text_lines(data: &[u8]) -> Vec<String> {
    split_lines(data)
        .into_iter()
        .map(|line| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            String::from_utf8_lossy(line).into_owned()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Env;
    use crate::fs::{OverlayFs, Vfs};
    use crate::io::{BytesInput, BytesOutput};
    use crate::system::NoSystem;
    use alloc::string::ToString;

    struct Harness {
        fs: OverlayFs,
        env: Env,
        stdin: BytesInput,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                fs: OverlayFs::memory(),
                env: Env::feos(),
                stdin: BytesInput::empty(),
            }
        }

        fn run(&mut self, name: &str, args: &[&str]) -> (i32, String, String) {
            let mut stdin = core::mem::replace(&mut self.stdin, BytesInput::empty());
            let mut stdout = BytesOutput::new();
            let mut stderr = BytesOutput::new();
            let args: Vec<String> = args.iter().map(|value| value.to_string()).collect();
            let mut ctx = Ctx {
                fs: &mut self.fs,
                env: &mut self.env,
                stdin: &mut stdin,
                stdout: &mut stdout,
                stderr: &mut stderr,
                system: &NoSystem,
            };
            let status = run(name, &args, &mut ctx);
            (
                status,
                String::from_utf8_lossy(stdout.bytes()).into_owned(),
                String::from_utf8_lossy(stderr.bytes()).into_owned(),
            )
        }

        fn run_stdin(&mut self, name: &str, args: &[&str], input: &[u8]) -> (i32, String, String) {
            self.stdin = BytesInput::new(input.to_vec());
            self.run(name, args)
        }
    }

    #[test]
    fn creates_lists_and_removes_files() {
        let mut h = Harness::new();
        assert_eq!(h.run("mkdir", &["-p", "a/b"]).0, 0);
        h.fs.write_file("/a/b/one.txt", b"one\n", false).unwrap();
        let (status, out, _) = h.run("ls", &["a/b"]);
        assert_eq!(status, 0);
        assert!(out.contains("one.txt"));

        let (status, _, _) = h.run("cp", &["a/b/one.txt", "a/b/two.txt"]);
        assert_eq!(status, 0);
        assert_eq!(h.fs.read_file("/a/b/two.txt").unwrap(), b"one\n");

        assert_eq!(h.run("mv", &["a/b/two.txt", "a/b/three.txt"]).0, 0);
        assert!(!h.fs.exists("/a/b/two.txt"));
        assert!(h.fs.exists("/a/b/three.txt"));

        assert_eq!(h.run("rm", &["-r", "a"]).0, 0);
        assert!(!h.fs.exists("/a"));
    }

    #[test]
    fn reads_and_slices_text() {
        let mut h = Harness::new();
        h.fs.write_file("/file.txt", b"one\ntwo\nthree\nfour\n", false)
            .unwrap();

        assert_eq!(h.run("cat", &["file.txt"]).1, "one\ntwo\nthree\nfour\n");
        assert_eq!(h.run("head", &["-n", "2", "file.txt"]).1, "one\ntwo\n");
        assert_eq!(h.run("tail", &["-n", "2", "file.txt"]).1, "three\nfour\n");
        assert_eq!(h.run("wc", &["-l", "file.txt"]).1.trim(), "4 file.txt");
    }

    #[test]
    fn filters_and_sorts_text() {
        let mut h = Harness::new();
        h.fs.write_file("/file.txt", b"pear\napple\napple\nbanana\n", false)
            .unwrap();

        assert_eq!(
            h.run("sort", &["file.txt"]).1,
            "apple\napple\nbanana\npear\n"
        );
        assert_eq!(
            h.run("sort", &["-u", "file.txt"]).1,
            "apple\nbanana\npear\n"
        );
        assert_eq!(
            h.run("uniq", &["-c", "file.txt"]).1,
            "      1 pear\n      2 apple\n      1 banana\n"
        );
        assert_eq!(
            h.run("grep", &["-n", "apple", "file.txt"]).1,
            "2:apple\n3:apple\n"
        );
        assert_eq!(
            h.run("grep", &["-v", "apple", "file.txt"]).1,
            "pear\nbanana\n"
        );
    }

    #[test]
    fn cuts_translates_and_sequences() {
        let mut h = Harness::new();
        h.fs.write_file("/csv", b"a,b,c\nd,e,f\n", false).unwrap();
        assert_eq!(h.run("cut", &["-d", ",", "-f", "2", "csv"]).1, "b\ne\n");
        assert_eq!(h.run_stdin("tr", &["a-z", "A-Z"], b"abc").1, "ABC");
        assert_eq!(h.run("seq", &["1", "3"]).1, "1\n2\n3\n");
    }

    #[test]
    fn hashes_and_encodes_data() {
        let mut h = Harness::new();
        h.fs.write_file("/hello", b"abc", false).unwrap();
        let (_, out, _) = h.run("sha256sum", &["hello"]);
        assert!(out.starts_with("ba7816bf8f01cfea414140de5dae2223"));
        assert_eq!(h.run_stdin("base64", &[], b"abc").1, "YWJj\n");
        assert_eq!(h.run_stdin("base64", &["-d"], b"YWJj\n").1, "abc");
    }

    #[test]
    fn evaluates_tests_and_expressions() {
        let mut h = Harness::new();
        h.fs.write_file("/file", b"x", false).unwrap();
        assert_eq!(h.run("test", &["-f", "file"]).0, 0);
        assert_eq!(h.run("test", &["-d", "file"]).0, 1);
        assert_eq!(h.run("test", &["3", "-gt", "2"]).0, 0);
        assert_eq!(h.run("expr", &["2", "+", "3", "*", "4"]).1, "20\n");
    }
}
