//! File-management utilities.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::opts::{bad_option, Opt, Opts};
use super::{
    error, format_size, parse_usize, path_error, usage, write_line, write_out, Ctx, MAX_WALK_DEPTH,
};
use crate::fs::{base_name, join as fs_join, FsError, Kind, Meta};

#[derive(Clone, Copy)]
struct LsOptions {
    long: bool,
    all: bool,
    almost_all: bool,
    human: bool,
    recursive: bool,
    directory: bool,
    reverse: bool,
    classify: bool,
    by_size: bool,
}

pub(super) fn ls(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut options = LsOptions {
        long: false,
        all: false,
        almost_all: false,
        human: false,
        recursive: false,
        directory: false,
        reverse: false,
        classify: false,
        by_size: false,
    };
    let mut opts = Opts::new(args, "laAhRdrF1S");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('l') => options.long = true,
            Opt::Flag('a') => options.all = true,
            Opt::Flag('A') => options.almost_all = true,
            Opt::Flag('h') => options.human = true,
            Opt::Flag('R') => options.recursive = true,
            Opt::Flag('d') => options.directory = true,
            Opt::Flag('r') => options.reverse = true,
            Opt::Flag('F') => options.classify = true,
            Opt::Flag('S') => options.by_size = true,
            Opt::Flag('1') => {}
            other => return bad_option(ctx, "ls", other),
        }
    }
    let mut paths = core::mem::take(&mut opts.operands);
    if paths.is_empty() {
        paths.push(String::from("."));
    }

    let mut status = 0;
    let mut first = true;
    let header = paths.len() > 1;
    // Files come first and directories get headers, as POSIX describes.
    let mut directories: Vec<&String> = Vec::new();
    for path in &paths {
        match ctx.fs.meta(path) {
            Ok(meta) if meta.kind == Kind::File || options.directory => {
                print_entry(ctx, path, meta, &options);
                first = false;
            }
            Ok(_) => directories.push(path),
            Err(err) => {
                path_error(ctx.stderr, "ls", path, err);
                status = 2;
            }
        }
    }
    for path in directories {
        if header {
            if !first {
                write_line(ctx.stdout, format_args!(""));
            }
            write_line(ctx.stdout, format_args!("{path}:"));
        }
        first = false;
        if let Err(err) = ls_directory(ctx, path, &options, 0) {
            path_error(ctx.stderr, "ls", path, err);
            status = 2;
        }
    }
    status
}

fn ls_directory(
    ctx: &mut Ctx<'_>,
    path: &str,
    options: &LsOptions,
    depth: usize,
) -> Result<(), FsError> {
    if depth > MAX_WALK_DEPTH {
        return Err(FsError::TooDeep);
    }
    let mut entries = ctx.fs.read_dir(path)?;
    if !options.all && !options.almost_all {
        entries.retain(|entry| !entry.name.starts_with('.'));
    }
    if options.by_size {
        entries.sort_by_key(|entry| core::cmp::Reverse(entry.size));
    }
    if options.all {
        for name in [".", ".."] {
            entries.insert(
                if name == "." { 0 } else { 1 },
                crate::fs::DirEntry {
                    name: String::from(name),
                    kind: Kind::Dir,
                    size: 0,
                },
            );
        }
    }
    if options.reverse {
        entries.reverse();
    }

    for entry in &entries {
        let meta = Meta {
            kind: entry.kind,
            size: entry.size,
        };
        let full = fs_join(path, &entry.name);
        print_entry_at(ctx, &full, &entry.name, meta, options);
    }

    if options.recursive {
        for entry in entries {
            if entry.kind != Kind::Dir || entry.name == "." || entry.name == ".." {
                continue;
            }
            if ctx.cancelled() {
                break;
            }
            let child = fs_join(path, &entry.name);
            write_line(ctx.stdout, format_args!(""));
            write_line(ctx.stdout, format_args!("{child}:"));
            ls_directory(ctx, &child, options, depth + 1)?;
        }
    }
    Ok(())
}

/// A name for a listing: escaped for a terminal, and as it is for anything reading it.
pub(super) fn listed_name<'a>(ctx: &Ctx<'_>, name: &'a str) -> alloc::borrow::Cow<'a, str> {
    if ctx.stdout.is_terminal() {
        feos_text::display_name(name)
    } else {
        alloc::borrow::Cow::Borrowed(name)
    }
}

/// `-rwxr-x---` from a mode.
fn mode_text(mode: u16) -> String {
    let kind = match mode & feos_fs::native::S_IFMT {
        feos_fs::native::S_IFDIR => 'd',
        feos_fs::native::S_IFLNK => 'l',
        _ => '-',
    };
    let mut text = String::new();
    text.push(kind);
    for shift in [6, 3, 0] {
        let bits = mode >> shift;
        text.push(if bits & 4 != 0 { 'r' } else { '-' });
        text.push(if bits & 2 != 0 { 'w' } else { '-' });
        text.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    text
}

fn print_entry(ctx: &mut Ctx<'_>, name: &str, meta: Meta, options: &LsOptions) {
    print_entry_at(ctx, name, name, meta, options);
}

fn print_entry_at(ctx: &mut Ctx<'_>, path: &str, name: &str, meta: Meta, options: &LsOptions) {
    let full = if options.long { ctx.fs.stat_full(path) } else { None };
    let name = listed_name(ctx, name);
    let suffix = if options.classify && meta.kind == Kind::Dir {
        "/"
    } else {
        ""
    };

    if options.long {
        let mode = if meta.kind == Kind::Dir {
            "drwxr-xr-x"
        } else {
            "-rw-r--r--"
        };
        let size = if options.human {
            format_size(meta.size)
        } else {
            format!("{}", meta.size)
        };
        match full {
            // Real owners, where there are such things.
            Some(stat) if stat.posix => {
                let mode = mode_text(stat.mode);
                let (links, uid, gid) = (stat.nlink, stat.uid, stat.gid);
                write_line(
                    ctx.stdout,
                    format_args!("{mode} {links} {uid} {gid} {size:>8} Jan  1 00:00 {name}{suffix}"),
                );
            }
            _ => write_line(
                ctx.stdout,
                format_args!("{mode} 1 root root {size:>8} Jan  1 00:00 {name}{suffix}"),
            ),
        }
    } else {
        write_line(ctx.stdout, format_args!("{name}{suffix}"));
    }
}

pub(super) fn cat(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut number = false;
    let mut number_nonblank = false;
    let mut squeeze = false;
    let mut show_ends = false;
    let mut opts = Opts::new(args, "nbsAEu");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('n') => number = true,
            Opt::Flag('b') => number_nonblank = true,
            Opt::Flag('s') => squeeze = true,
            Opt::Flag('A' | 'E') => show_ends = true,
            Opt::Flag('u') => {}
            other => return bad_option(ctx, "cat", other),
        }
    }
    let mut files = core::mem::take(&mut opts.operands);
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let mut status = 0;
    let mut line_number = 1usize;
    let mut blank_run = false;
    let plain = !(number || number_nonblank || squeeze || show_ends);

    for file in files {
        if plain {
            // Plain `cat` streams, so a file bigger than the heap still works.
            if let Err(err) = copy_to_stdout(ctx, &file) {
                path_error(ctx.stderr, "cat", &file, err);
                status = 1;
            }
            continue;
        }

        let data = match super::read_path(ctx, &file) {
            Ok(data) => data,
            Err(err) => {
                path_error(ctx.stderr, "cat", &file, err);
                status = 1;
                continue;
            }
        };

        for line in super::split_lines(&data) {
            let has_newline = line.ends_with(b"\n");
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            let blank = body.is_empty();

            if squeeze {
                if blank && blank_run {
                    continue;
                }
                blank_run = blank;
            }

            let numbered = number_nonblank && !blank || number && !number_nonblank;
            if numbered {
                write_out(ctx.stdout, format_args!("{line_number:>6}\t"));
                line_number += 1;
            }

            ctx.stdout.write(body);
            if show_ends {
                write_out(ctx.stdout, format_args!("$"));
            }
            if has_newline {
                write_out(ctx.stdout, format_args!("\n"));
            }
        }
    }
    status
}

/// Copies a file or standard input to standard output in small pieces.
fn copy_to_stdout(ctx: &mut Ctx<'_>, file: &str) -> Result<(), FsError> {
    let mut chunk = [0u8; 4096];
    let mut offset = 0usize;
    loop {
        let read = if file == "-" {
            ctx.stdin.read(&mut chunk)
        } else {
            ctx.fs.read_at(file, offset, &mut chunk)?
        };
        if read == 0 {
            return Ok(());
        }
        ctx.stdout.write(&chunk[..read]);
        offset += read;
        if ctx.cancelled() {
            return Ok(());
        }
    }
}

pub(super) fn mkdir(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut parents = false;
    let mut verbose = false;
    let mut opts = Opts::new(args, "pvm:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('p') => parents = true,
            Opt::Flag('v') => verbose = true,
            Opt::Value('m', _) => {}
            other => return bad_option(ctx, "mkdir", other),
        }
    }
    let dirs = core::mem::take(&mut opts.operands);
    if dirs.is_empty() {
        return usage(ctx.stderr, "mkdir", "mkdir [-pv] DIR...");
    }

    let mut status = 0;
    for dir in dirs {
        let result = if parents {
            create_parents(ctx, &dir)
        } else {
            ctx.fs.create_dir(&dir)
        };
        match result {
            Ok(()) if verbose => {
                write_line(ctx.stdout, format_args!("mkdir: created directory '{dir}'"))
            }
            Ok(()) => {}
            Err(err) => {
                path_error(ctx.stderr, "mkdir", &dir, err);
                status = 1;
            }
        }
    }
    status
}

fn create_parents(ctx: &mut Ctx<'_>, path: &str) -> Result<(), FsError> {
    let absolute = ctx.fs.resolve(path);
    if absolute == "/" {
        return Ok(());
    }
    let mut current = String::from("/");
    for part in absolute.split('/').filter(|part| !part.is_empty()) {
        current = fs_join(&current, part);
        match ctx.fs.meta(&current) {
            Ok(meta) if meta.kind == Kind::Dir => {}
            Ok(_) => return Err(FsError::NotDir),
            Err(FsError::NotFound) => ctx.fs.create_dir(&current)?,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub(super) fn rmdir(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut parents = false;
    let mut verbose = false;
    let mut opts = Opts::new(args, "pv");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('p') => parents = true,
            Opt::Flag('v') => verbose = true,
            other => return bad_option(ctx, "rmdir", other),
        }
    }
    let dirs = core::mem::take(&mut opts.operands);
    if dirs.is_empty() {
        return usage(ctx.stderr, "rmdir", "rmdir [-pv] DIR...");
    }

    let mut status = 0;
    for dir in dirs {
        let mut current = ctx.fs.resolve(&dir);
        loop {
            match ctx.fs.remove_dir(&current) {
                Ok(()) => {
                    if verbose {
                        write_line(
                            ctx.stdout,
                            format_args!("rmdir: removing directory '{current}'"),
                        );
                    }
                }
                Err(err) => {
                    path_error(ctx.stderr, "rmdir", &current, err);
                    status = 1;
                    break;
                }
            }
            if !parents || current == "/" {
                break;
            }
            let parent = crate::fs::parent(&current).to_string();
            if parent == current {
                break;
            }
            current = parent;
        }
    }
    status
}

pub(super) fn rm(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut recursive = false;
    let mut force = false;
    let mut verbose = false;
    let mut dir_ok = false;
    let mut opts = Opts::new(args, "rRfvdi");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('r' | 'R') => recursive = true,
            Opt::Flag('f') => force = true,
            Opt::Flag('v') => verbose = true,
            Opt::Flag('d') => dir_ok = true,
            Opt::Flag('i') => {}
            other => return bad_option(ctx, "rm", other),
        }
    }
    let paths = core::mem::take(&mut opts.operands);
    if paths.is_empty() {
        if force {
            return 0;
        }
        return usage(ctx.stderr, "rm", "rm [-rfdv] PATH...");
    }

    let mut status = 0;
    for path in paths {
        if path == "." || path == ".." || path.ends_with("/.") || path.ends_with("/..") {
            error(
                ctx.stderr,
                "rm",
                &format!("refusing to remove '.' or '..': skipping '{path}'"),
            );
            status = 1;
            continue;
        }
        if path == "/" || path == "/*" {
            error(ctx.stderr, "rm", "refusing to remove '/'");
            status = 1;
            continue;
        }
        let absolute = ctx.fs.resolve(&path);
        if absolute == "/" {
            error(ctx.stderr, "rm", "refusing to remove '/'");
            status = 1;
            continue;
        }
        if path.ends_with('/') && ctx.fs.meta(&path) == Err(FsError::NotDir) {
            path_error(ctx.stderr, "rm", &path, FsError::NotDir);
            status = 1;
            continue;
        }
        match remove_any(ctx, &absolute, recursive, dir_ok, 0) {
            Ok(()) => {
                if verbose {
                    write_line(ctx.stdout, format_args!("removed '{path}'"));
                }
            }
            Err(FsError::NotFound) if force => {}
            Err(err) => {
                path_error(ctx.stderr, "rm", &path, err);
                status = 1;
            }
        }
    }
    status
}

fn remove_any(
    ctx: &mut Ctx<'_>,
    absolute: &str,
    recursive: bool,
    dir_ok: bool,
    depth: usize,
) -> Result<(), FsError> {
    if depth > MAX_WALK_DEPTH {
        return Err(FsError::TooDeep);
    }
    let meta = ctx.fs.meta(absolute)?;
    match meta.kind {
        Kind::File => ctx.fs.remove_file(absolute),
        Kind::Dir => {
            if !recursive {
                if dir_ok {
                    return ctx.fs.remove_dir(absolute);
                }
                return Err(FsError::IsDir);
            }
            let entries = ctx.fs.read_dir(absolute)?;
            for entry in entries {
                let child = fs_join(absolute, &entry.name);
                remove_any(ctx, &child, true, false, depth + 1)?;
            }
            ctx.fs.remove_dir(absolute)
        }
    }
}

pub(super) fn cp(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut recursive = false;
    let mut no_clobber = false;
    let mut verbose = false;
    let mut opts = Opts::new(args, "rRnvfipa");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('r' | 'R' | 'a') => recursive = true,
            Opt::Flag('n') => no_clobber = true,
            Opt::Flag('v') => verbose = true,
            Opt::Flag('f' | 'i' | 'p') => {}
            other => return bad_option(ctx, "cp", other),
        }
    }
    let mut operands = core::mem::take(&mut opts.operands);
    if operands.len() < 2 {
        return usage(ctx.stderr, "cp", "cp [-rRfvn] SRC... DEST");
    }

    let Some(dest) = operands.pop() else {
        return 2;
    };
    let dest_meta = ctx.fs.meta(&dest).ok();
    if operands.len() > 1 && !matches!(dest_meta, Some(meta) if meta.kind == Kind::Dir) {
        error(ctx.stderr, "cp", "target is not a directory");
        return 1;
    }

    let mut status = 0;
    for source in operands {
        match copy_one(ctx, &source, &dest, recursive, no_clobber) {
            Ok(()) => {
                if verbose {
                    write_line(ctx.stdout, format_args!("'{source}' -> '{dest}'"));
                }
            }
            Err(err) => {
                path_error(ctx.stderr, "cp", &source, err);
                status = 1;
            }
        }
    }
    status
}

fn copy_one(
    ctx: &mut Ctx<'_>,
    source: &str,
    dest: &str,
    recursive: bool,
    no_clobber: bool,
) -> Result<(), FsError> {
    let source_abs = ctx.fs.resolve(source);
    let dest_abs = ctx.fs.resolve(dest);
    let source_meta = ctx.fs.meta(&source_abs)?;
    let dest_meta = ctx.fs.meta(&dest_abs).ok();

    let target = match dest_meta {
        Some(meta) if meta.kind == Kind::Dir => fs_join(&dest_abs, base_name(&source_abs)),
        _ => dest_abs.clone(),
    };

    if source_meta.kind == Kind::Dir && !recursive {
        return Err(FsError::IsDir);
    }

    let inside = format!("{source_abs}/");
    if target.starts_with(&inside) {
        return Err(FsError::InvalidPath);
    }

    if source_abs == target {
        return Err(FsError::InvalidPath);
    }

    copy_tree(ctx, &source_abs, &target, no_clobber, 0)
}

fn copy_tree(
    ctx: &mut Ctx<'_>,
    source: &str,
    target: &str,
    no_clobber: bool,
    depth: usize,
) -> Result<(), FsError> {
    if depth > MAX_WALK_DEPTH {
        return Err(FsError::TooDeep);
    }
    let meta = ctx.fs.meta(source)?;
    match meta.kind {
        Kind::File => {
            if no_clobber && ctx.fs.exists(target) {
                return Ok(());
            }
            if let Ok(dest_meta) = ctx.fs.meta(target) {
                if dest_meta.kind == Kind::Dir {
                    return Err(FsError::IsDir);
                }
            }
            let data = ctx.fs.read_file(source)?;
            ctx.fs.write_file(target, &data, false)
        }
        Kind::Dir => {
            match ctx.fs.meta(target) {
                Ok(meta) if meta.kind == Kind::Dir => {}
                Ok(_) => return Err(FsError::NotDir),
                Err(FsError::NotFound) => ctx.fs.create_dir(target)?,
                Err(err) => return Err(err),
            }
            let entries = ctx.fs.read_dir(source)?;
            for entry in entries {
                let child_source = fs_join(source, &entry.name);
                let child_target = fs_join(target, &entry.name);
                copy_tree(ctx, &child_source, &child_target, no_clobber, depth + 1)?;
            }
            Ok(())
        }
    }
}

pub(super) fn mv(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut no_clobber = false;
    let mut verbose = false;
    let mut opts = Opts::new(args, "nvfi");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('n') => no_clobber = true,
            Opt::Flag('v') => verbose = true,
            Opt::Flag('f' | 'i') => {}
            other => return bad_option(ctx, "mv", other),
        }
    }
    let mut operands = core::mem::take(&mut opts.operands);
    if operands.len() < 2 {
        return usage(ctx.stderr, "mv", "mv [-fvn] SRC... DEST");
    }

    let Some(dest) = operands.pop() else {
        return 2;
    };
    let dest_meta = ctx.fs.meta(&dest).ok();
    if operands.len() > 1 && !matches!(dest_meta, Some(meta) if meta.kind == Kind::Dir) {
        error(ctx.stderr, "mv", "target is not a directory");
        return 1;
    }

    let mut status = 0;
    for source in operands {
        match move_one(ctx, &source, &dest, no_clobber) {
            Ok(true) => {
                if verbose {
                    write_line(ctx.stdout, format_args!("'{source}' -> '{dest}'"));
                }
            }
            Ok(false) => {}
            Err(err) => {
                path_error(ctx.stderr, "mv", &source, err);
                status = 1;
            }
        }
    }
    status
}

fn move_one(
    ctx: &mut Ctx<'_>,
    source: &str,
    dest: &str,
    no_clobber: bool,
) -> Result<bool, FsError> {
    let source_abs = ctx.fs.resolve(source);
    let dest_abs = ctx.fs.resolve(dest);
    let source_meta = ctx.fs.meta(&source_abs)?;
    let dest_meta = ctx.fs.meta(&dest_abs).ok();

    let target = match dest_meta {
        Some(meta) if meta.kind == Kind::Dir => fs_join(&dest_abs, base_name(&source_abs)),
        _ => dest_abs.clone(),
    };

    if source_abs == target {
        return Ok(false);
    }
    let inside = format!("{source_abs}/");
    if target.starts_with(&inside) {
        return Err(FsError::InvalidPath);
    }

    if let Ok(meta) = ctx.fs.meta(&target) {
        if no_clobber {
            return Ok(false);
        }
        if meta.kind == Kind::Dir {
            return Err(FsError::Exists);
        }
        if source_meta.kind == Kind::Dir {
            return Err(FsError::NotDir);
        }
        ctx.fs.remove_file(&target)?;
    }

    ctx.fs.rename(&source_abs, &target)?;
    Ok(true)
}

pub(super) fn touch(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut no_create = false;
    let mut opts = Opts::new(args, "camt:d:r:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('c') => no_create = true,
            Opt::Flag('a' | 'm') | Opt::Value('t' | 'd' | 'r', _) => {}
            other => return bad_option(ctx, "touch", other),
        }
    }
    let files = core::mem::take(&mut opts.operands);
    if files.is_empty() {
        return usage(ctx.stderr, "touch", "touch [-c] FILE...");
    }

    let mut status = 0;
    for file in files {
        if ctx.fs.exists(&file) {
            if let Some(now) = ctx.system.unix_time() {
                let times = feos_fs::NodeTimes {
                    modified: Some(now),
                    accessed: Some(now),
                    created: None,
                };
                // A filesystem with no timestamps has nothing to update.
                match ctx.fs.set_times(&file, times) {
                    Ok(()) | Err(FsError::NotSupported) => {}
                    Err(err) => {
                        path_error(ctx.stderr, "touch", &file, err);
                        status = 1;
                    }
                }
            }
            continue;
        }
        if no_create {
            continue;
        }
        if let Err(err) = ctx.fs.write_file(&file, b"", false) {
            path_error(ctx.stderr, "touch", &file, err);
            status = 1;
        }
    }
    status
}

pub(super) fn stat(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    if args.is_empty() {
        return usage(ctx.stderr, "stat", "stat PATH...");
    }
    let mut status = 0;
    for path in args {
        let meta = match ctx.fs.meta(path) {
            Ok(meta) => meta,
            Err(err) => {
                path_error(ctx.stderr, "stat", path, err);
                status = 1;
                continue;
            }
        };
        let absolute = ctx.fs.resolve(path);
        let kind = if meta.kind == Kind::Dir {
            "directory"
        } else {
            "regular file"
        };
        write_line(ctx.stdout, format_args!("  File: {absolute}"));
        if let Some(full) = ctx.fs.stat_full(path) {
            let made_up = if full.posix { "" } else { " (made up: this filesystem has no owners)" };
            write_line(
                ctx.stdout,
                format_args!(
                    "  Mode: {:04o} {}  Uid: {}  Gid: {}  Links: {}  Inode: {}{made_up}",
                    full.mode & 0o7777,
                    mode_text(full.mode),
                    full.uid,
                    full.gid,
                    full.nlink,
                    full.ino,
                ),
            );
        }
        write_line(
            ctx.stdout,
            format_args!("  Size: {:<10} Type: {kind}", meta.size),
        );
        if let Ok(target) = ctx.fs.read_link(path) {
            write_line(ctx.stdout, format_args!("  Link: {target}"));
        }
        if let Some(modified) = ctx.fs.times(path).and_then(|times| times.modified) {
            let date = feos_hal::time::DateTime::from_unix(modified);
            write_line(
                ctx.stdout,
                format_args!(
                    "Modify: {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                    date.year, date.month, date.day, date.hour, date.minute, date.second
                ),
            );
        }
        if meta.kind == Kind::Dir {
            if let Ok(entries) = ctx.fs.read_dir(&absolute) { write_line(ctx.stdout, format_args!("  Entries: {}", entries.len())) }
        }

    }
    status
}

pub(super) fn tree(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut all = false;
    let mut max_depth = usize::MAX;
    let mut opts = Opts::new(args, "aL:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('a') => all = true,
            Opt::Value('L', value) => max_depth = parse_usize(value).unwrap_or(usize::MAX),
            other => return bad_option(ctx, "tree", other),
        }
    }
    let mut paths = core::mem::take(&mut opts.operands);
    if paths.is_empty() {
        paths.push(String::from("."));
    }

    let mut status = 0;
    for path in paths {
        match ctx.fs.meta(&path) {
            Ok(meta) if meta.kind == Kind::Dir => {
                write_line(ctx.stdout, format_args!("{path}"));
                let mut dirs = 0usize;
                let mut files = 0usize;
                if let Err(err) =
                    tree_walk(ctx, &path, "", 0, max_depth, all, &mut dirs, &mut files)
                {
                    path_error(ctx.stderr, "tree", &path, err);
                    status = 1;
                } else {
                    write_line(
                        ctx.stdout,
                        format_args!("\n{dirs} directories, {files} files"),
                    );
                }
            }
            Ok(_) => write_line(ctx.stdout, format_args!("{path}  [file]")),
            Err(err) => {
                path_error(ctx.stderr, "tree", &path, err);
                status = 1;
            }
        }
    }
    status
}

#[allow(clippy::too_many_arguments)]
fn tree_walk(
    ctx: &mut Ctx<'_>,
    path: &str,
    prefix: &str,
    depth: usize,
    max_depth: usize,
    all: bool,
    dirs: &mut usize,
    files: &mut usize,
) -> Result<(), FsError> {
    if depth >= max_depth {
        return Ok(());
    }
    if depth > MAX_WALK_DEPTH {
        return Err(FsError::TooDeep);
    }
    let mut entries = ctx.fs.read_dir(path)?;
    if !all {
        entries.retain(|entry| !entry.name.starts_with('.'));
    }
    let count = entries.len();
    for (index, entry) in entries.iter().enumerate() {
        let last = index + 1 == count;
        let branch = if last { "`-- " } else { "|-- " };
        if entry.kind == Kind::Dir {
            *dirs += 1;
            let shown = listed_name(ctx, &entry.name);
            write_line(ctx.stdout, format_args!("{prefix}{branch}{shown}/"));
            let next = if last {
                format!("{prefix}    ")
            } else {
                format!("{prefix}|   ")
            };
            let child = fs_join(path, &entry.name);
            tree_walk(ctx, &child, &next, depth + 1, max_depth, all, dirs, files)?;
        } else {
            *files += 1;
            let shown = listed_name(ctx, &entry.name);
            write_line(ctx.stdout, format_args!("{prefix}{branch}{shown}"));
        }
    }
    Ok(())
}

pub(super) fn du(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut summary = false;
    let mut human = false;
    let mut all = false;
    let mut opts = Opts::new(args, "sahk");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('s') => summary = true,
            Opt::Flag('h') => human = true,
            Opt::Flag('a') => all = true,
            Opt::Flag('k') => {}
            other => return bad_option(ctx, "du", other),
        }
    }
    let mut paths = core::mem::take(&mut opts.operands);
    if paths.is_empty() {
        paths.push(String::from("."));
    }

    let mut status = 0;
    for path in paths {
        let mut total = 0usize;
        if let Err(err) = du_walk(ctx, &path, summary, all, human, &mut total, 0) {
            path_error(ctx.stderr, "du", &path, err);
            status = 1;
        }
    }
    status
}

fn du_walk(
    ctx: &mut Ctx<'_>,
    path: &str,
    summary: bool,
    all: bool,
    human: bool,
    total: &mut usize,
    depth: usize,
) -> Result<usize, FsError> {
    if depth > MAX_WALK_DEPTH {
        return Err(FsError::TooDeep);
    }
    let root = depth == 0;
    let meta = ctx.fs.meta(path)?;
    let mut size = match meta.kind {
        Kind::File => meta.size,
        Kind::Dir => 0,
    };

    if meta.kind == Kind::Dir {
        let entries = ctx.fs.read_dir(path)?;
        for entry in entries {
            let child = fs_join(path, &entry.name);
            size = size.saturating_add(du_walk(ctx, &child, summary, all, human, total, depth + 1)?);
        }
    }

    *total = total.saturating_add(match meta.kind {
        Kind::File => meta.size,
        Kind::Dir => 0,
    });

    if (root && summary) || (!summary && (meta.kind == Kind::Dir || all)) {
        let shown = if human {
            format_size(size)
        } else {
            format!("{size}")
        };
        write_line(ctx.stdout, format_args!("{shown}\t{path}"));
    }
    Ok(size)
}

/// `sync`: everything so far reaches the disk before this returns.
pub(super) fn sync(_args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    match ctx.fs.sync() {
        Ok(()) => 0,
        Err(err) => {
            error(ctx.stderr, "sync", err.text());
            1
        }
    }
}

/// `mount`: lists what's attached. Attaching is left to the kernel for now.
pub(super) fn mount(_args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    for mount in ctx.fs.mounts() {
        let mode = if mount.read_only || mount.stats.read_only {
            "ro"
        } else {
            "rw"
        };
        write_line(
            ctx.stdout,
            format_args!("{} on {} type {} ({mode})", mount.source, mount.path, mount.stats.kind),
        );
    }
    0
}

/// `ln [-s] TARGET LINK`. A hard link takes a filesystem with inodes: FAT has none.
pub(super) fn ln(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut symbolic = false;
    let mut opts = Opts::new(args, "s");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('s') => symbolic = true,
            other => return bad_option(ctx, "ln", other),
        }
    }
    let operands = core::mem::take(&mut opts.operands);
    let [target, link] = operands.as_slice() else {
        return usage(ctx.stderr, "ln", "ln [-s] TARGET LINK");
    };
    let result = if symbolic {
        ctx.fs.symlink(target, link)
    } else {
        ctx.fs.hard_link(target, link)
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            path_error(ctx.stderr, "ln", link, err);
            1
        }
    }
}

/// A mode from `chmod`'s argument: octal, or `[ugoa]*[+-=][rwx]*`, in terms of the old one.
fn parse_mode(text: &str, old: u16) -> Option<u16> {
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        return u16::from_str_radix(text, 8).ok().filter(|mode| *mode <= 0o7777);
    }
    let mut mode = old & 0o7777;
    for clause in text.split(',') {
        let at = clause.find(['+', '-', '='])?;
        let (who, rest) = clause.split_at(at);
        let mut mask = 0u16;
        for ch in who.chars() {
            mask |= match ch {
                'u' => 0o700,
                'g' => 0o070,
                'o' => 0o007,
                'a' => 0o777,
                _ => return None,
            };
        }
        if who.is_empty() {
            mask = 0o777;
        }
        let mut chars = rest.chars();
        let op = chars.next()?;
        let mut bits = 0u16;
        for ch in chars {
            bits |= match ch {
                'r' => 0o444,
                'w' => 0o222,
                'x' => 0o111,
                _ => return None,
            };
        }
        match op {
            '+' => mode |= bits & mask,
            '-' => mode &= !(bits & mask),
            _ => mode = (mode & !mask) | (bits & mask),
        }
    }
    Some(mode)
}

/// `chmod MODE FILE...`.
pub(super) fn chmod(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let [mode, files @ ..] = args else {
        return usage(ctx.stderr, "chmod", "chmod MODE FILE...");
    };
    if files.is_empty() {
        return usage(ctx.stderr, "chmod", "chmod MODE FILE...");
    }
    let mut status = 0;
    for file in files {
        let old = ctx.fs.stat_full(file).map_or(0, |stat| stat.mode);
        let Some(new) = parse_mode(mode, old) else {
            error(ctx.stderr, "chmod", &format!("invalid mode: {mode}"));
            return 1;
        };
        if let Err(err) = ctx.fs.chmod(file, new) {
            path_error(ctx.stderr, "chmod", file, err);
            status = 1;
        }
    }
    status
}

/// `chown OWNER[:GROUP] FILE...`, by number: there's no user database.
pub(super) fn chown(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let [owner, files @ ..] = args else {
        return usage(ctx.stderr, "chown", "chown UID[:GID] FILE...");
    };
    if files.is_empty() {
        return usage(ctx.stderr, "chown", "chown UID[:GID] FILE...");
    }
    let (uid, gid) = owner.split_once(':').unwrap_or((owner, ""));
    let number = |text: &str| -> Result<Option<u32>, ()> {
        if text.is_empty() {
            Ok(None)
        } else {
            text.parse().map(Some).map_err(|_| ())
        }
    };
    let (Ok(uid), Ok(gid)) = (number(uid), number(gid)) else {
        error(ctx.stderr, "chown", &format!("not numbers: {owner}"));
        return 1;
    };
    let mut status = 0;
    for file in files {
        if let Err(err) = ctx.fs.chown(file, uid, gid) {
            path_error(ctx.stderr, "chown", file, err);
            status = 1;
        }
    }
    status
}

/// `fsck [-r] PATH`: checks the filesystem holding a path, on one that has a checker.
pub(super) fn fsck(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut repair = false;
    let mut opts = Opts::new(args, "r");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('r') => repair = true,
            other => return bad_option(ctx, "fsck", other),
        }
    }
    let operands = core::mem::take(&mut opts.operands);
    let [path] = operands.as_slice() else {
        return usage(ctx.stderr, "fsck", "fsck [-r] PATH");
    };
    match ctx.fs.fsck(path, repair) {
        Ok(report) => {
            for problem in &report.errors {
                write_line(ctx.stdout, format_args!("fsck: {problem}"));
            }
            let verdict = if report.errors.is_empty() {
                "clean"
            } else if report.clean() {
                "repaired"
            } else {
                "errors"
            };
            write_line(
                ctx.stdout,
                format_args!(
                    "fsck: {path}: {verdict}: {} files, {} directories, {} of {} blocks used, {} repaired",
                    report.files, report.directories, report.used_blocks, report.total_blocks, report.repaired
                ),
            );
            i32::from(!report.clean())
        }
        Err(err) => {
            path_error(ctx.stderr, "fsck", path, err);
            8
        }
    }
}

/// `readlink LINK`.
pub(super) fn readlink(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let [link] = args else {
        return usage(ctx.stderr, "readlink", "readlink LINK");
    };
    match ctx.fs.read_link(link) {
        Ok(target) => {
            write_line(ctx.stdout, format_args!("{target}"));
            0
        }
        Err(err) => {
            path_error(ctx.stderr, "readlink", link, err);
            1
        }
    }
}

pub(super) fn df(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let human = args.iter().any(|arg| arg.contains('h'));
    let mounts = ctx.fs.mounts();
    if !mounts.is_empty() {
        write_line(
            ctx.stdout,
            format_args!("Filesystem      Size      Used     Avail Use% Mounted on"),
        );
        for mount in mounts {
            let unit = mount.stats.block_size as usize;
            let total = mount.stats.total_blocks as usize * unit;
            let free = mount.stats.free_blocks as usize * unit;
            let used = total.saturating_sub(free);
            let percent = used.saturating_mul(100).checked_div(total).unwrap_or(0);
            let show = |bytes: usize| {
                if human {
                    format_size(bytes)
                } else {
                    format!("{bytes}")
                }
            };
            write_line(
                ctx.stdout,
                format_args!(
                    "{:<15} {:>4} {:>9} {:>9} {percent:>3}% {}",
                    mount.source,
                    show(total),
                    show(used),
                    show(free),
                    mount.path
                ),
            );
        }
        return 0;
    }
    let Some(memory) = ctx.system.memory() else {
        error(ctx.stderr, "df", "no memory accounting available");
        return 1;
    };
    let (total, used, free) = if human {
        (
            format_size(memory.total),
            format_size(memory.used),
            format_size(memory.free),
        )
    } else {
        (
            format!("{}", memory.total),
            format!("{}", memory.used),
            format!("{}", memory.free),
        )
    };
    let percent = memory.used.saturating_mul(100).checked_div(memory.total).unwrap_or(0);
    write_line(
        ctx.stdout,
        format_args!("Filesystem      Size      Used     Avail Use% Mounted on"),
    );
    write_line(
        ctx.stdout,
        format_args!("feos         {total:>8}  {used:>8}  {free:>8}  {percent:>3}% /"),
    );
    0
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn modes_parse_both_ways() {
        assert_eq!(parse_mode("640", 0), Some(0o640));
        assert_eq!(parse_mode("0755", 0), Some(0o755));
        assert_eq!(parse_mode("9", 0), None);
        assert_eq!(parse_mode("u+x", 0o644), Some(0o744));
        assert_eq!(parse_mode("go-r", 0o644), Some(0o600));
        assert_eq!(parse_mode("a=r", 0o777), Some(0o444));
        assert_eq!(parse_mode("+x", 0o644), Some(0o755));
        assert_eq!(parse_mode("u+r,g-r", 0o040), Some(0o400));
        assert_eq!(parse_mode("q+x", 0), None);
        assert_eq!(mode_text(0o100644), "-rw-r--r--");
        assert_eq!(mode_text(0o040750), "drwxr-x---");
        assert_eq!(mode_text(0o120777), "lrwxrwxrwx");
    }
}
