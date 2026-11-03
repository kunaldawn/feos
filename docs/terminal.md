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

Pipelines are executed by running each stage, buffering its output and feeding
it to the next. This is not a streaming pipe, but it is deterministic and works
with every finite coreutil. Commands from the app registry (`ps`, `tone`,
`files`, `player`, …) are intentionally blocked inside pipelines and file
redirections: they print through the global console and cannot be captured.

`&` launches a registered app through the kernel process scheduler when the
command is external. Builtins and coreutils run in the foreground because they
share the shell's overlay filesystem and variables; a future job-control layer
would need a per-process VFS handle to change that.

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
