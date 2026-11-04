//! The option parser every command shares.
//!
//! It implements the getopt conventions: clustered flags (`-la`), attached and
//! separate values (`-d,` and `-d ,`), `--` to end the options, a lone `-` as
//! an operand, and options mixed freely with operands.

use alloc::string::String;
use alloc::vec::Vec;

use super::{error, Ctx};

/// One parsed option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Opt<'a> {
    /// A flag such as `-l`.
    Flag(char),
    /// A flag with a value such as `-n 3` or `-n3`.
    Value(char, &'a str),
    /// A long option such as `--count` or `--color=never`.
    Long(&'a str, Option<&'a str>),
    /// A flag the specification does not list.
    Unknown(char),
    /// A value flag at the end of the arguments.
    MissingValue(char),
}

/// Iterates over `args`, collecting operands into [`Opts::operands`].
pub(super) struct Opts<'a> {
    args: &'a [String],
    index: usize,
    cluster: usize,
    spec: &'static str,
    numbers_are_operands: bool,
    stop_at_operand: bool,
    finished: bool,
    /// Non-option arguments seen so far.
    pub operands: Vec<String>,
}

impl<'a> Opts<'a> {
    /// `spec` lists the flags, with `:` after each flag that takes a value.
    pub fn new(args: &'a [String], spec: &'static str) -> Self {
        Self {
            args,
            index: 0,
            cluster: 0,
            spec,
            numbers_are_operands: false,
            stop_at_operand: false,
            finished: false,
            operands: Vec::new(),
        }
    }

    /// Treats `-1` as an operand, for commands that take negative numbers.
    pub fn numbers_are_operands(mut self) -> Self {
        self.numbers_are_operands = true;
        self
    }

    /// Stops at the first operand, for commands that run another command.
    pub fn stop_at_operand(mut self) -> Self {
        self.stop_at_operand = true;
        self
    }

    /// The arguments after the option that ended parsing.
    pub fn rest(&self) -> &'a [String] {
        self.args.get(self.index..).unwrap_or(&[])
    }

    fn lookup(&self, flag: char) -> Option<bool> {
        let mut chars = self.spec.chars().peekable();
        while let Some(known) = chars.next() {
            let takes_value = chars.peek() == Some(&':');
            if takes_value {
                chars.next();
            }
            if known == flag && known != ':' {
                return Some(takes_value);
            }
        }
        None
    }

    /// Returns the next option, or `None` when only operands remain.
    pub fn next(&mut self) -> Option<Opt<'a>> {
        loop {
            if self.finished {
                return None;
            }
            let args = self.args;
            let Some(arg) = args.get(self.index) else {
                self.finished = true;
                return None;
            };

            if self.cluster == 0 {
                if arg == "--" {
                    self.index += 1;
                    self.finished = true;
                    if !self.stop_at_operand {
                        self.operands.extend_from_slice(self.rest());
                        self.index = args.len();
                    }
                    return None;
                }
                if let Some(long) = arg.strip_prefix("--") {
                    self.index += 1;
                    return Some(match long.split_once('=') {
                        Some((name, value)) => Opt::Long(name, Some(value)),
                        None => Opt::Long(long, None),
                    });
                }
                let numeric = self.numbers_are_operands
                    && arg.as_bytes().get(1).is_some_and(u8::is_ascii_digit);
                if arg.len() < 2 || !arg.starts_with('-') || numeric {
                    if self.stop_at_operand {
                        self.finished = true;
                        return None;
                    }
                    self.operands.push(arg.clone());
                    self.index += 1;
                    continue;
                }
                self.cluster = 1;
            }

            let Some(flag) = arg.get(self.cluster..).and_then(|rest| rest.chars().next()) else {
                self.cluster = 0;
                self.index += 1;
                continue;
            };
            self.cluster += flag.len_utf8();
            let remainder = arg.get(self.cluster..).unwrap_or("");

            return Some(match self.lookup(flag) {
                None => {
                    if remainder.is_empty() {
                        self.cluster = 0;
                        self.index += 1;
                    }
                    Opt::Unknown(flag)
                }
                Some(false) => {
                    if remainder.is_empty() {
                        self.cluster = 0;
                        self.index += 1;
                    }
                    Opt::Flag(flag)
                }
                Some(true) => {
                    self.cluster = 0;
                    self.index += 1;
                    if !remainder.is_empty() {
                        Opt::Value(flag, remainder)
                    } else if let Some(value) = args.get(self.index) {
                        self.index += 1;
                        Opt::Value(flag, value)
                    } else {
                        Opt::MissingValue(flag)
                    }
                }
            });
        }
    }
}

/// Reports an option a command does not accept, returning status 2.
pub(super) fn bad_option(ctx: &mut Ctx<'_>, cmd: &str, opt: Opt<'_>) -> i32 {
    let message = match opt {
        Opt::Flag(flag) | Opt::Value(flag, _) | Opt::Unknown(flag) => {
            alloc::format!("invalid option -- '{flag}'")
        }
        Opt::MissingValue(flag) => alloc::format!("option requires an argument -- '{flag}'"),
        Opt::Long(name, _) => alloc::format!("unrecognized option '--{name}'"),
    };
    error(ctx.stderr, cmd, &message);
    2
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn parses_clusters_values_and_operands() {
        let args = strings(&["-ab", "file", "-c3", "-d", "x", "-", "--long=1", "--", "-z"]);
        let mut opts = Opts::new(&args, "abc:d:");
        assert_eq!(opts.next(), Some(Opt::Flag('a')));
        assert_eq!(opts.next(), Some(Opt::Flag('b')));
        assert_eq!(opts.next(), Some(Opt::Value('c', "3")));
        assert_eq!(opts.next(), Some(Opt::Value('d', "x")));
        assert_eq!(opts.next(), Some(Opt::Long("long", Some("1"))));
        assert_eq!(opts.next(), None);
        assert_eq!(opts.operands, ["file", "-", "-z"]);
    }

    #[test]
    fn reports_unknown_flags_and_missing_values() {
        let args = strings(&["-x", "-c"]);
        let mut opts = Opts::new(&args, "c:");
        assert_eq!(opts.next(), Some(Opt::Unknown('x')));
        assert_eq!(opts.next(), Some(Opt::MissingValue('c')));
        assert_eq!(opts.next(), None);
    }

    #[test]
    fn negative_numbers_can_be_operands() {
        let args = strings(&["3", "-1", "1", "-w"]);
        let mut opts = Opts::new(&args, "w").numbers_are_operands();
        assert_eq!(opts.next(), Some(Opt::Flag('w')));
        assert_eq!(opts.next(), None);
        assert_eq!(opts.operands, ["3", "-1", "1"]);
    }

    #[test]
    fn can_stop_at_the_first_operand() {
        let args = strings(&["-n", "2", "cmd", "-x"]);
        let mut opts = Opts::new(&args, "n:").stop_at_operand();
        assert_eq!(opts.next(), Some(Opt::Value('n', "2")));
        assert_eq!(opts.next(), None);
        assert_eq!(opts.rest(), ["cmd", "-x"]);
    }
}
