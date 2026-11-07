# The FEOS terminal

FEOS has two front ends for the same shell engine: the serial console that
appears after logging off the desktop, and the **Terminal** window in the
desktop Start menu. Both are Bash-compatible shells with a POSIX coreutils
library. The code is split into three layers:

```
apps/shell        console + app-registry adapter
      |
     feos-sh      lexer, parser, expansion, builtins, line editor
      |
feos-coreutils    commands, writable overlay VFS, text/system services
      |
   feos-fs        the board's compiled-in read-only image
```

Nothing in the shell knows about QEMU, the PL011 or a particular board. The
adapter supplies console streams, platform facts (`uname`, `uptime`, `free`)
and an external-command host that resolves names through the normal app
registry. Keyboard input reaches the desktop from the serial console and, when
QEMU is started with `-device virtio-keyboard-device`, from the graphical
window as well.

## Supported language

| Feature | Notes |
|---|---|
| Quoting | `'single'`, `"double"`, backslash escapes, line continuations |
| Variables | `NAME=value`, `$NAME`, `${NAME}`, `${#NAME}`, `${NAME:-x}`, `${NAME:=x}`, `${NAME:+x}` |
| Special parameters | `$?`, `$$`, `$#`, `$0`–`$9` |
| Command substitution | `$(command)` |
| Arithmetic | `$((expression))` with `+ - * / %`, parentheses and variables; integer only |
| Globbing | `*`, `?`, `[abc]`, `[a-z]`, `[!abc]` |
| Word splitting | Unquoted expansions split on whitespace; quoted expansions do not |
| Lists | `;`, newline, `&&`, `\|\|`, `&` |
| Pipelines | `cmd1 \| cmd2 \| cmd3` |
| Redirection | `<`, `>`, `>>`, `2>`, `2>>`, `2>&1`, `&>`, `&>>` |
| Compound commands | `if`/`elif`/`else`/`fi`, `for`/`in`/`do`/`done`, `while`, `until`, `{ ...; }`, `( ... )` |
| Builtins | `cd`, `pwd`, `export`, `unset`, `set`, `exit`, `return`, `source`/`.`, `eval`, `alias`, `unalias`, `history`, `help`, `type`, `command`, `read`, `break`, `continue`, `clear`, `sh`/`bash` |
| Interactive editing | Arrows, Home/End, Delete, history, Ctrl-A/E/U/K/W, Ctrl-L, Ctrl-C, Ctrl-D, Ctrl-R, Tab |
| Zoom (desktop window) | Ctrl-`=`/Ctrl-`+` zooms in, Ctrl-`-` zooms out, Ctrl-`0` resets (1x–4x) |
| Scripts | `sh script`, `source script`, `bash -c '...'` |
| Timing | `time pipeline` and `time -p`, a reserved word as in Bash, reporting on standard error. CPU time is the shell's and the children it outlived; all of it counts as user, with sys 0, as the kernel doesn't tell them apart |
| Traps | `trap` fires EXIT, ERR, INT, HUP, TERM and QUIT |
| Job control | `&`, `jobs`, `fg`, `bg`, `wait`, `kill %n`, Ctrl-Z |

Pipelines are executed by running each stage, buffering its output and feeding
it to the next. This is not a streaming pipe, but it is deterministic and works
with every finite coreutil. Commands from the app registry (`ps`, `tone`,
`files`, `player`, …) are intentionally blocked inside pipelines and file
redirections: they print through the global console and cannot be captured.

`&` launches a registered app through the kernel process scheduler when the
command is external. Builtins and coreutils run in the foreground because they
share the shell's overlay filesystem and variables; a future job-control layer
would need a per-process VFS handle to change that.

## Signals and traps

There's no general signal delivery. A shell that sets a trap tells the kernel which signals
it catches (`Processes::catch_signals`), and only those are queued for it, waking it if it's
blocked. Any other process is simply killed by `kill -TERM`, `-HUP`, `-QUIT` or `-INT`. A trap
runs between commands, so one that arrives during a command waits for it, except that `wait`
and `fg` give way. It also runs with nothing else going on: at an idle prompt in a Terminal or
on the serial console. Closing a Terminal window runs its HUP trap and then its EXIT trap.
Ctrl-\\ ends a foreground process with status 131, and then the QUIT trap runs. The desktop
session keeps Ctrl-\\ for logging off, so this is the serial shell's.

## Stopping a job

Ctrl-Z stops a foreground process (`Processes::set_stopped`, on it and what it started), adds
it to the job table and returns status 148. The shell keeps the job's pipes, so `fg` picks up
its output and input where they were, and `bg` hands its output to a small task, `job-io`,
that copies it to the terminal. Stopped jobs are killed when their shell goes.

Only a process can be stopped: an app from the registry or a user program, with its streams on
the terminal. A coreutil or builtin runs on the shell's own task, and a pipeline or a redirected
command has a shell waiting on it that a stopped command would hang, so Ctrl-Z does nothing
to those.

## Standard input

A process run from a shell has a standard input: `Context::read_stdin` and
`Context::try_read_stdin` for an app, the `read` call for a user program. Behind it is a pipe that
the shell feeds. From a terminal, that's cooked lines: typing is echoed, Enter ends a line,
Backspace and Ctrl-U edit it, and Ctrl-D is end of file. Ctrl-C still kills the process.

The terminal is only read while the process is asking (`PipeWriter::reader_wants`), so keys typed
ahead of a command that never reads stay for the shell. Whatever was fed and not read goes back
when it ends. `nc`, given no text or file to send, sends what it reads, and a peer closing ends it.

## Names, selection and Unicode

Filesystems keep names as they are, control characters included. `feos_text::Escaped` and
`display_name` turn them into `\n`, `\e`, `\x7f` and so on wherever they're shown: `ls`, `tree` and
`find` when writing to a terminal, tab completion and the prompt. Piped, those commands give the
raw name, as a program reading the list needs it.

In a Terminal window, dragging selects, a double click selects a word, and Ctrl+Shift+C or the
right button copies to the shared clipboard (4 KiB). Ctrl+Shift+V, or the right button with nothing
selected, pastes. Selections follow their text as it scrolls.

Cells have `feos_text::char_width`'s width, by ranges rather than a full table: 2 for East Asian wide
characters and emoji, 0 for combining marks, 1 otherwise. A wide character that doesn't fit wraps
whole, and the line editor counts columns the same way. Combining marks are dropped, as a cell holds
one character. Glyphs exist for Latin-1, Latin Extended-A, Greek, Cyrillic and box drawing: see
[the font](architecture.md#the-font). Anything else, CJK included, is an empty box in its proper cells.

## Owners and links

The shell and its coreutils run as root: `id` says so, and nothing checks them. `chmod`, `chown` (by number)
and `ln` without `-s` work on the [native filesystem](architecture.md#the-native-filesystem), and refuse on FAT,
exFAT and RAM mounts, which have no such thing. `ls -l` and `stat` show real owners and modes where there are
any, and say when they're made up. `fsck [-r] PATH` checks a native filesystem. To run a user program as somebody
else: `/bin/runas UID GID PROGRAM [ARGS]`. Permissions apply to it.

## Writable filesystem

The board compiles a read-only image into the kernel. The terminal wraps it in
`feos-coreutils::OverlayFs`:

- reads fall through to the base image;
- writes are stored in an in-RAM `BTreeMap` keyed by absolute path;
- deleting a base file or directory leaves a tombstone so the base stays
  untouched;
- copying or renaming a base subtree materialises it in RAM;
- the overlay is reset on reboot.

There are no symlinks, hard links, permissions, timestamps or user quotas yet.
`ls -l`, `stat` and `df` print sensible fixed values.

Each shell instance owns its overlay. The serial terminal and the desktop
window therefore start from the same read-only image but do not currently see
each other's writes; that would need a shared VFS handle in `Platform`.

## Coreutils coverage

```
basename  cat      cmp      cp       cut      dirname  df       du
echo      env      expr     false    find     free     grep     head
hexdump   hostname id       ls       mkdir    mv       nl       nproc
paste     printenv printf   pwd      rev      rm       rmdir    seq
sha256sum sort     stat     tac      tail     tee      test     touch
tr        tree     true     uname    uniq     uptime   wc       whoami
yes
```

Use `help <command>` or `coreutils <command>` for usage. The implementations
are intentionally practical rather than byte-for-byte GNU clones; for example
`grep` uses a small backtracking regex engine with literals, `.`, `*`, `+`,
`?`, classes and anchors, but no groups or alternation.

`yes` is capped at 10,000 lines because FEOS has no signal delivery yet; a
truly unbounded `yes` would make the serial console unrecoverable.

## Examples

```bash
# A script stored in the read-only image can be sourced
source /examples/notes.txt

# Count files and show the largest names
ls /docs | wc -l
ls -l /media | sort -k 5 -n

# Redirection and pipelines
printf 'pear\napple\napple\n' | sort -u > /tmp/fruit
cat /tmp/fruit | grep apple

# Loops and arithmetic
for n in 1 2 3 4 5; do
  if test $((n % 2)) -eq 0; then
    echo "$n is even"
  fi
done

# Command substitution
echo "today: $(uname -s) $(uname -m)"
```

## References

The design borrows ideas from projects that already solve pieces of this
problem, but none of them is a drop-in fit for a bare-metal `no_std` kernel:

- [uutils/coreutils](https://github.com/uutils/coreutils) — the Rust GNU
  coreutils rewrite; command semantics and option coverage.
- [brush](https://github.com/reubeno/brush) — a Bash-compatible shell in Rust;
  parser and compatibility-test ideas.
- [yash-syntax](https://crates.io/crates/yash-syntax) — a POSIX shell parser.
- [armybox](https://crates.io/crates/armybox) — a `no_std` BusyBox/Toybox
  clone; small-utility organization.
- [BusyBox](https://busybox.net/) and [toybox](https://landley.net/toybox/) —
  the classic small-system utility set.
