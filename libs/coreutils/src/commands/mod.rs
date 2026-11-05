//! Command registry and dispatch for [`crate::run`].

mod archive;
mod awk;
mod diff;
mod edit;
mod files;
mod find;
mod less;
mod more;
mod opts;
mod sed;
mod system;
mod text;
mod tty;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::env::Env;
use crate::fs::{FsError, Vfs};
use crate::io::{Input, Output, MAX_STREAM_BYTES};
use crate::system::System;

/// Deepest directory tree the recursive utilities will walk.
pub(super) const MAX_WALK_DEPTH: usize = 64;

/// How deeply commands may run commands, as `xargs env xargs ...` does.
const MAX_COMMAND_DEPTH: u32 = 8;

/// The entry point of one command.
pub type CommandFn = fn(&[String], &mut Ctx<'_>) -> i32;

/// A command name, its help text, aliases and implementation.
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
    /// The implementation, so a listed command always dispatches.
    pub run: CommandFn,
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
    /// How many commands are running this one. Start at zero.
    pub depth: u32,
}

impl Ctx<'_> {
    /// True when the command should stop: its output is gone because the sink
    /// overflowed or the shell cancelled it.
    pub fn cancelled(&self) -> bool {
        self.stdout.is_broken()
    }
}

/// Every command implemented by this crate, in help order.
pub static COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "ls",
        aliases: &["dir"],
        usage: "ls [-la1hRFr] [path...]",
        description: "list directory contents",
        run: files::ls,
    },
    CommandSpec {
        name: "cat",
        aliases: &[],
        usage: "cat [-nbsA] [file...]",
        description: "concatenate files",
        run: files::cat,
    },
    CommandSpec {
        name: "echo",
        aliases: &[],
        usage: "echo [-neE] [text...]",
        description: "print text",
        run: text::echo,
    },
    CommandSpec {
        name: "printf",
        aliases: &[],
        usage: "printf FORMAT [arg...]",
        description: "format and print data",
        run: text::printf,
    },
    CommandSpec {
        name: "pwd",
        aliases: &[],
        usage: "pwd",
        description: "print the working directory",
        run: system::pwd,
    },
    CommandSpec {
        name: "mkdir",
        aliases: &[],
        usage: "mkdir [-pv] DIR...",
        description: "create directories",
        run: files::mkdir,
    },
    CommandSpec {
        name: "rmdir",
        aliases: &[],
        usage: "rmdir [-pv] DIR...",
        description: "remove empty directories",
        run: files::rmdir,
    },
    CommandSpec {
        name: "rm",
        aliases: &[],
        usage: "rm [-rfdv] PATH...",
        description: "remove files and directories",
        run: files::rm,
    },
    CommandSpec {
        name: "cp",
        aliases: &[],
        usage: "cp [-rRfvni] SRC... DEST",
        description: "copy files and directories",
        run: files::cp,
    },
    CommandSpec {
        name: "mv",
        aliases: &[],
        usage: "mv [-fvn] SRC... DEST",
        description: "move or rename files",
        run: files::mv,
    },
    CommandSpec {
        name: "touch",
        aliases: &[],
        usage: "touch [-c] FILE...",
        description: "create empty files",
        run: files::touch,
    },
    CommandSpec {
        name: "head",
        aliases: &[],
        usage: "head [-n N|-c N] [file...]",
        description: "first lines of files",
        run: text::head,
    },
    CommandSpec {
        name: "tail",
        aliases: &[],
        usage: "tail [-n N|-c N] [file...]",
        description: "last lines of files",
        run: text::tail,
    },
    CommandSpec {
        name: "wc",
        aliases: &[],
        usage: "wc [-lwcm] [file...]",
        description: "count lines, words and bytes",
        run: text::wc,
    },
    CommandSpec {
        name: "grep",
        aliases: &[],
        usage: "grep [-ivnclLqwxFr] PATTERN [file...]",
        description: "search text with a pattern",
        run: text::grep,
    },
    CommandSpec {
        name: "sort",
        aliases: &[],
        usage: "sort [-rnufb] [-t C] [-k N] [file...]",
        description: "sort lines",
        run: text::sort,
    },
    CommandSpec {
        name: "uniq",
        aliases: &[],
        usage: "uniq [-cdui] [file]",
        description: "filter repeated lines",
        run: text::uniq,
    },
    CommandSpec {
        name: "cut",
        aliases: &[],
        usage: "cut [-d C] [-f LIST] [-c LIST] [file...]",
        description: "select fields or columns",
        run: text::cut,
    },
    CommandSpec {
        name: "tr",
        aliases: &[],
        usage: "tr [-dsc] SET1 [SET2]",
        description: "translate or delete bytes",
        run: text::tr,
    },
    CommandSpec {
        name: "seq",
        aliases: &[],
        usage: "seq [-w] [-s SEP] [first [step]] last",
        description: "print a sequence of numbers",
        run: text::seq,
    },
    CommandSpec {
        name: "basename",
        aliases: &[],
        usage: "basename PATH [SUFFIX]",
        description: "strip the directory from a path",
        run: more::basename,
    },
    CommandSpec {
        name: "dirname",
        aliases: &[],
        usage: "dirname PATH",
        description: "strip the last path component",
        run: more::dirname,
    },
    CommandSpec {
        name: "nl",
        aliases: &[],
        usage: "nl [-b a|t] [-w N] [file...]",
        description: "number lines",
        run: text::nl,
    },
    CommandSpec {
        name: "rev",
        aliases: &[],
        usage: "rev [file...]",
        description: "reverse each line",
        run: text::rev,
    },
    CommandSpec {
        name: "tac",
        aliases: &[],
        usage: "tac [file...]",
        description: "print lines in reverse order",
        run: text::tac,
    },
    CommandSpec {
        name: "paste",
        aliases: &[],
        usage: "paste [-d LIST] [-s] [file...]",
        description: "merge lines of files",
        run: text::paste,
    },
    CommandSpec {
        name: "tee",
        aliases: &[],
        usage: "tee [-a] [file...]",
        description: "copy input to files and stdout",
        run: text::tee,
    },
    CommandSpec {
        name: "cmp",
        aliases: &[],
        usage: "cmp [-sl] FILE1 FILE2",
        description: "compare two files byte by byte",
        run: text::cmp,
    },
    CommandSpec {
        name: "find",
        aliases: &[],
        usage: "find [path...] [-name PAT] [-type f|d] [-maxdepth N]",
        description: "walk a directory tree",
        run: find::find,
    },
    CommandSpec {
        name: "stat",
        aliases: &[],
        usage: "stat PATH...",
        description: "show file metadata",
        run: files::stat,
    },
    CommandSpec {
        name: "tree",
        aliases: &[],
        usage: "tree [-a] [-L N] [path]",
        description: "display a directory tree",
        run: files::tree,
    },
    CommandSpec {
        name: "du",
        aliases: &[],
        usage: "du [-sah] [path...]",
        description: "estimate file space usage",
        run: files::du,
    },
    CommandSpec {
        name: "appinstall",
        aliases: &[],
        usage: "appinstall FILE [SHA256]",
        description: "install a .fapp package after checking its digest",
        run: archive::appinstall,
    },
    CommandSpec {
        name: "sync",
        aliases: &[],
        usage: "sync",
        description: "write pending changes to disk",
        run: files::sync,
    },
    CommandSpec {
        name: "mount",
        aliases: &[],
        usage: "mount",
        description: "list mounted filesystems",
        run: files::mount,
    },
    CommandSpec {
        name: "ln",
        aliases: &[],
        usage: "ln -s TARGET LINK",
        description: "make a symbolic link",
        run: files::ln,
    },
    CommandSpec {
        name: "readlink",
        aliases: &[],
        usage: "readlink LINK",
        description: "print where a symbolic link points",
        run: files::readlink,
    },
    CommandSpec {
        name: "df",
        aliases: &[],
        usage: "df [-h]",
        description: "show filesystem space",
        run: files::df,
    },
    CommandSpec {
        name: "env",
        aliases: &[],
        usage: "env",
        description: "print the environment",
        run: system::env,
    },
    CommandSpec {
        name: "printenv",
        aliases: &[],
        usage: "printenv [NAME...]",
        description: "print environment variables",
        run: system::printenv,
    },
    CommandSpec {
        name: "uname",
        aliases: &[],
        usage: "uname [-asnrvm]",
        description: "print system information",
        run: system::uname,
    },
    CommandSpec {
        name: "whoami",
        aliases: &[],
        usage: "whoami",
        description: "print the current user",
        run: system::whoami,
    },
    CommandSpec {
        name: "id",
        aliases: &[],
        usage: "id",
        description: "print user and group ids",
        run: system::id,
    },
    CommandSpec {
        name: "hostname",
        aliases: &[],
        usage: "hostname",
        description: "print the host name",
        run: system::hostname,
    },
    CommandSpec {
        name: "nproc",
        aliases: &[],
        usage: "nproc",
        description: "print the number of CPUs",
        run: system::nproc,
    },
    CommandSpec {
        name: "arch",
        aliases: &[],
        usage: "arch",
        description: "print the machine architecture",
        run: system::arch,
    },
    CommandSpec {
        name: "uptime",
        aliases: &[],
        usage: "uptime",
        description: "show how long the system has been up",
        run: system::uptime,
    },
    CommandSpec {
        name: "free",
        aliases: &[],
        usage: "free [-h]",
        description: "show memory usage",
        run: system::free,
    },
    CommandSpec {
        name: "hexdump",
        aliases: &["xxd"],
        usage: "hexdump [-C] [-n N] [file...]",
        description: "dump bytes as hexadecimal",
        run: text::hexdump,
    },
    CommandSpec {
        name: "base64",
        aliases: &[],
        usage: "base64 [-d] [-w N] [file...]",
        description: "encode or decode base64",
        run: text::base64,
    },
    CommandSpec {
        name: "sha256sum",
        aliases: &[],
        usage: "sha256sum [file...]",
        description: "print SHA-256 checksums",
        run: text::sha256sum,
    },
    CommandSpec {
        name: "test",
        aliases: &[],
        usage: "test EXPRESSION",
        description: "evaluate a conditional expression",
        run: system::test,
    },
    CommandSpec {
        name: "[",
        aliases: &[],
        usage: "[ EXPRESSION ]",
        description: "evaluate a conditional expression",
        run: system::bracket,
    },
    CommandSpec {
        name: "expr",
        aliases: &[],
        usage: "expr ARG...",
        description: "evaluate an integer expression",
        run: system::expr,
    },
    CommandSpec {
        name: "true",
        aliases: &[":"],
        usage: "true",
        description: "do nothing successfully",
        run: system::run_true,
    },
    CommandSpec {
        name: "false",
        aliases: &[],
        usage: "false",
        description: "do nothing unsuccessfully",
        run: system::run_false,
    },
    CommandSpec {
        name: "awk",
        aliases: &[],
        usage: "awk [-F fs] [-v var=value] [-f file | program] [file...]",
        description: "scan and process patterns in text",
        run: awk::awk,
    },
    CommandSpec {
        name: "edit",
        aliases: &["nano"],
        usage: "edit [file]",
        description: "edit a text file (^S save, ^Q quit, ^F find)",
        run: edit::edit,
    },
    CommandSpec {
        name: "less",
        aliases: &["more"],
        usage: "less [file]",
        description: "page through text (q quits, / searches)",
        run: less::less,
    },
    CommandSpec {
        name: "yes",
        aliases: &[],
        usage: "yes [text]",
        description: "repeat a line (capped for safety)",
        run: text::yes,
    },
    CommandSpec {
        name: "sed",
        aliases: &[],
        usage: "sed [-nEi] [-e SCRIPT] [-f FILE] [SCRIPT] [file...]",
        description: "stream editor",
        run: sed::sed,
    },
    CommandSpec {
        name: "diff",
        aliases: &[],
        usage: "diff [-uq] [-U N] FILE1 FILE2",
        description: "compare files line by line",
        run: diff::diff,
    },
    CommandSpec {
        name: "xargs",
        aliases: &[],
        usage: "xargs [-0t] [-n N] [-I STR] [-d C] [command [arg...]]",
        description: "build commands from standard input",
        run: more::xargs,
    },
    CommandSpec {
        name: "tar",
        aliases: &[],
        usage: "tar [-ctxvzf] [-C DIR] [-f FILE] [path...]",
        description: "create, list and extract archives",
        run: archive::tar,
    },
    CommandSpec {
        name: "gzip",
        aliases: &[],
        usage: "gzip [-dkcf] [file...]",
        description: "compress files",
        run: archive::gzip,
    },
    CommandSpec {
        name: "zcat",
        aliases: &[],
        usage: "zcat [file...]",
        description: "print decompressed files",
        run: archive::zcat,
    },
    CommandSpec {
        name: "gunzip",
        aliases: &[],
        usage: "gunzip [-kcf] [file...]",
        description: "decompress files",
        run: archive::gunzip,
    },
    CommandSpec {
        name: "od",
        aliases: &[],
        usage: "od [-A d|o|x|n] [-t x1|o1|d1|c] [-c] [-x] [-N N] [-j N] [file...]",
        description: "dump bytes in octal and other formats",
        run: more::od,
    },
    CommandSpec {
        name: "fold",
        aliases: &[],
        usage: "fold [-bs] [-w N] [file...]",
        description: "wrap long lines",
        run: more::fold,
    },
    CommandSpec {
        name: "comm",
        aliases: &[],
        usage: "comm [-123] FILE1 FILE2",
        description: "compare two sorted files",
        run: more::comm,
    },
    CommandSpec {
        name: "split",
        aliases: &[],
        usage: "split [-l N] [-b N] [-a N] [-d] [file [prefix]]",
        description: "split a file into pieces",
        run: more::split,
    },
    CommandSpec {
        name: "dd",
        aliases: &[],
        usage: "dd [if=F] [of=F] [bs=N] [count=N] [skip=N] [seek=N] [status=none]",
        description: "copy and convert blocks",
        run: more::dd,
    },
    CommandSpec {
        name: "strings",
        aliases: &[],
        usage: "strings [-n N] [file...]",
        description: "print printable character runs",
        run: more::strings,
    },
    CommandSpec {
        name: "realpath",
        aliases: &[],
        usage: "realpath [-me] PATH...",
        description: "print the resolved path",
        run: more::realpath,
    },
    CommandSpec {
        name: "mktemp",
        aliases: &[],
        usage: "mktemp [-d] [-p DIR] [TEMPLATE]",
        description: "create a unique temporary file",
        run: more::mktemp,
    },
    CommandSpec {
        name: "file",
        aliases: &[],
        usage: "file [-b] PATH...",
        description: "identify file types",
        run: more::file,
    },
    CommandSpec {
        name: "date",
        aliases: &[],
        usage: "date [-u] [-d @SECONDS] [+FORMAT]",
        description: "print the date and time",
        run: more::date,
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
    match command(name) {
        Some(spec) => (spec.run)(args, ctx),
        None => {
            error(ctx.stderr, name, "command not found");
            127
        }
    }
}

/// Runs a command on behalf of another command, bounding the nesting.
pub(super) fn run_nested(name: &str, args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if ctx.depth >= MAX_COMMAND_DEPTH {
        error(ctx.stderr, name, "commands are nested too deeply");
        return 126;
    }
    ctx.depth += 1;
    let status = run(name, args, ctx);
    ctx.depth -= 1;
    status
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
        ctx.stdin.try_read_to_end(MAX_STREAM_BYTES)
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
                depth: 0,
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
        assert_eq!(h.run("expr", &["2", "+", "3", "*", "4"]).1, "14\n");
    }

    #[test]
    fn every_listed_command_dispatches() {
        for spec in COMMANDS {
            for name in core::iter::once(&spec.name).chain(spec.aliases) {
                let mut h = Harness::new();
                let (status, _, err) = h.run(name, &[]);
                assert_ne!(status, 127, "{name} does not dispatch: {err}");
                assert!(!err.contains("command not found"), "{name}: {err}");
                assert!(is_command(name));
            }
        }
    }

    #[test]
    fn nested_commands_are_bounded() {
        let mut h = Harness::new();
        let args = alloc::vec!["xargs"; 100];
        let (status, _, err) = h.run_stdin("xargs", &args, b"a\n");
        assert_ne!(status, 0);
        assert!(err.contains("nested too deeply"), "{err}");
    }

    #[test]
    fn tar_extracts_links_and_refuses_to_leave_the_directory() {
        let tree: &'static feos_fs::Vfs =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(feos_fs::Vfs::in_memory(1 << 20)));
        let mut h = Harness::new();
        h.fs = OverlayFs::new(Some(tree));

        let mut image = alloc::vec![0u8; 8192];
        let mut writer = feos_archive::TarWriter::new(&mut image);
        writer.write_file("real.txt", b"contents").unwrap();
        writer.write_symlink("link.txt", "real.txt").unwrap();
        writer.write_file("../escaped.txt", b"out").unwrap();
        let len = writer.finish().unwrap();
        h.fs.write_file("/a.tar", &image[..len], false).unwrap();
        h.fs.create_dir("/out").unwrap();

        let (status, _, err) = h.run("tar", &["-xf", "/a.tar", "-C", "/out"]);
        assert_eq!(status, 2, "one member was refused");
        assert!(err.contains("escaped"), "{err}");
        assert_eq!(h.fs.read_file("/out/link.txt").unwrap(), b"contents");
        assert_eq!(h.fs.read_link("/out/link.txt").unwrap(), "real.txt");
        assert!(!h.fs.exists("/escaped.txt"));
    }

    #[test]
    fn appinstall_needs_a_digest_that_matches() {
        let tree: &'static feos_fs::Vfs =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(feos_fs::Vfs::in_memory(1 << 20)));
        let mut h = Harness::new();
        h.fs = OverlayFs::new(Some(tree));
        let mut image = alloc::vec![0u8; 2048];
        let mut writer = feos_archive::ZipWriter::new(&mut image);
        writer
            .write_stored("manifest.yaml", b"id: demo\nname: Demo\nversion: \"1\"\nentry: ps\n")
            .unwrap();
        let len = writer.finish().unwrap();
        let package = &image[..len];
        h.fs.write_file("/p.fapp", package, false).unwrap();
        let digest = crate::sha256::hex(&crate::sha256::sha256(package));

        assert_eq!(h.run("appinstall", &["/p.fapp"]).0, 1);
        let wrong = "0".repeat(64);
        assert_eq!(h.run("appinstall", &["/p.fapp", &wrong]).0, 1);
        assert!(!h.fs.exists("/apps/demo.fapp"));

        assert_eq!(h.run("appinstall", &["/p.fapp", &digest]).0, 0);
        assert_eq!(h.fs.read_file("/apps/demo.fapp").unwrap(), package);

        // A digest beside the package works too.
        h.fs.write_file("/p.fapp.sha256", digest.as_bytes(), false).unwrap();
        assert_eq!(h.run("appinstall", &["/p.fapp"]).0, 0);
    }
}
