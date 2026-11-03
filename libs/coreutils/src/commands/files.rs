//! File-management utilities.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{
    error, format_size, is_option, parse_usize, path_error, usage, write_line, write_out, Ctx,
};
use crate::fs::{base_name, join as fs_join, FsError, Kind, Meta};
use crate::glob::glob_match;

#[derive(Clone, Copy)]
struct LsOptions {
    long: bool,
    all: bool,
    human: bool,
    recursive: bool,
    directory: bool,
    reverse: bool,
    classify: bool,
}

pub(super) fn ls(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut options = LsOptions {
        long: false,
        all: false,
        human: false,
        recursive: false,
        directory: false,
        reverse: false,
        classify: false,
    };
    let mut paths: Vec<String> = Vec::new();
    let mut parse_options = true;

    for arg in args {
        if parse_options && arg == "--" {
            parse_options = false;
        } else if parse_options && is_option(arg) {
            for flag in arg[1..].chars() {
                match flag {
                    'l' => options.long = true,
                    'a' => options.all = true,
                    'h' => options.human = true,
                    'R' => options.recursive = true,
                    'd' => options.directory = true,
                    'r' => options.reverse = true,
                    'F' => options.classify = true,
                    '1' => {}
                    other => {
                        error(ctx.stderr, "ls", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            paths.push(arg.clone());
        }
    }

    if paths.is_empty() {
        paths.push(String::from("."));
    }

    let mut status = 0;
    for (index, path) in paths.iter().enumerate() {
        let header = paths.len() > 1;
        match ctx.fs.meta(path) {
            Ok(meta) if meta.kind == Kind::File || options.directory => {
                if header {
                    write_line(ctx.stdout, format_args!("{path}:"));
                }
                print_entry(ctx, base_name(path), meta, &options);
            }
            Ok(_) => {
                if header {
                    if index > 0 {
                        write_line(ctx.stdout, format_args!(""));
                    }
                    write_line(ctx.stdout, format_args!("{path}:"));
                }
                if let Err(err) = ls_directory(ctx, path, &options) {
                    path_error(ctx.stderr, "ls", path, err);
                    status = 1;
                }
            }
            Err(err) => {
                path_error(ctx.stderr, "ls", path, err);
                status = 1;
            }
        }
    }
    status
}

fn ls_directory(ctx: &mut Ctx<'_>, path: &str, options: &LsOptions) -> Result<(), FsError> {
    let mut entries = ctx.fs.read_dir(path)?;
    if options.all {
        entries.insert(
            0,
            crate::fs::DirEntry {
                name: String::from("."),
                kind: Kind::Dir,
                size: 0,
            },
        );
        entries.insert(
            0,
            crate::fs::DirEntry {
                name: String::from(".."),
                kind: Kind::Dir,
                size: 0,
            },
        );
    }
    if options.reverse {
        entries.reverse();
    }

    for entry in &entries {
        let meta = Meta {
            kind: entry.kind,
            size: entry.size,
        };
        print_entry(ctx, &entry.name, meta, options);
    }

    if options.recursive {
        for entry in entries {
            if entry.kind != Kind::Dir || entry.name == "." || entry.name == ".." {
                continue;
            }
            let child = fs_join(path, &entry.name);
            write_line(ctx.stdout, format_args!(""));
            write_line(ctx.stdout, format_args!("{child}:"));
            ls_directory(ctx, &child, options)?;
        }
    }
    Ok(())
}

fn print_entry(ctx: &mut Ctx<'_>, name: &str, meta: Meta, options: &LsOptions) {
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
        write_line(
            ctx.stdout,
            format_args!("{mode} 1 feos feos {size:>8} Jan  1 00:00 {name}{suffix}"),
        );
    } else {
        write_line(ctx.stdout, format_args!("{name}{suffix}"));
    }
}

pub(super) fn cat(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut number = false;
    let mut number_nonblank = false;
    let mut squeeze = false;
    let mut show_ends = false;
    let mut files: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) && arg != "-" {
            for flag in arg[1..].chars() {
                match flag {
                    'n' => number = true,
                    'b' => number_nonblank = true,
                    's' => squeeze = true,
                    'A' => show_ends = true,
                    'E' => show_ends = true,
                    other => {
                        error(ctx.stderr, "cat", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            files.push(arg.clone());
        }
    }
    if files.is_empty() {
        files.push(String::from("-"));
    }

    let mut status = 0;
    let mut line_number = 1usize;
    let mut blank_run = false;

    for file in files {
        let data = match super::read_path(ctx, &file) {
            Ok(data) => data,
            Err(err) => {
                path_error(ctx.stderr, "cat", &file, err);
                status = 1;
                continue;
            }
        };

        if !(number || number_nonblank || squeeze || show_ends) {
            ctx.stdout.write(&data);
            continue;
        }

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

pub(super) fn mkdir(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut parents = false;
    let mut verbose = false;
    let mut dirs: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) {
            for flag in arg[1..].chars() {
                match flag {
                    'p' => parents = true,
                    'v' => verbose = true,
                    other => {
                        error(ctx.stderr, "mkdir", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            dirs.push(arg.clone());
        }
    }
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
    let mut dirs: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) {
            for flag in arg[1..].chars() {
                match flag {
                    'p' => parents = true,
                    'v' => verbose = true,
                    other => {
                        error(ctx.stderr, "rmdir", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            dirs.push(arg.clone());
        }
    }
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
    let mut paths: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) && arg != "-" {
            for flag in arg[1..].chars() {
                match flag {
                    'r' | 'R' => recursive = true,
                    'f' => force = true,
                    'v' => verbose = true,
                    'd' => dir_ok = true,
                    other => {
                        error(ctx.stderr, "rm", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            paths.push(arg.clone());
        }
    }
    if paths.is_empty() {
        if force {
            return 0;
        }
        return usage(ctx.stderr, "rm", "rm [-rfdv] PATH...");
    }

    let mut status = 0;
    for path in paths {
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
        match remove_any(ctx, &absolute, recursive, dir_ok) {
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
) -> Result<(), FsError> {
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
                remove_any(ctx, &child, true, false)?;
            }
            ctx.fs.remove_dir(absolute)
        }
    }
}

pub(super) fn cp(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut recursive = false;
    let mut no_clobber = false;
    let mut verbose = false;
    let mut operands: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) && arg != "-" {
            for flag in arg[1..].chars() {
                match flag {
                    'r' | 'R' => recursive = true,
                    'n' => no_clobber = true,
                    'v' => verbose = true,
                    'f' | 'i' | 'p' => {}
                    other => {
                        error(ctx.stderr, "cp", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            operands.push(arg.clone());
        }
    }
    if operands.len() < 2 {
        return usage(ctx.stderr, "cp", "cp [-rRfvn] SRC... DEST");
    }

    let dest = operands.pop().unwrap();
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

    copy_tree(ctx, &source_abs, &target, no_clobber)
}

fn copy_tree(
    ctx: &mut Ctx<'_>,
    source: &str,
    target: &str,
    no_clobber: bool,
) -> Result<(), FsError> {
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
                copy_tree(ctx, &child_source, &child_target, no_clobber)?;
            }
            Ok(())
        }
    }
}

pub(super) fn mv(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut no_clobber = false;
    let mut verbose = false;
    let mut operands: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) && arg != "-" {
            for flag in arg[1..].chars() {
                match flag {
                    'n' => no_clobber = true,
                    'v' => verbose = true,
                    'f' | 'i' => {}
                    other => {
                        error(ctx.stderr, "mv", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            operands.push(arg.clone());
        }
    }
    if operands.len() < 2 {
        return usage(ctx.stderr, "mv", "mv [-fvn] SRC... DEST");
    }

    let dest = operands.pop().unwrap();
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
        ctx.fs.remove_file(&target)?;
    }

    let _ = source_meta;
    ctx.fs.rename(&source_abs, &target)?;
    Ok(true)
}

pub(super) fn touch(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut no_create = false;
    let mut files: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) {
            for flag in arg[1..].chars() {
                match flag {
                    'c' => no_create = true,
                    'a' | 'm' | 't' | 'd' | 'r' => {}
                    other => {
                        error(ctx.stderr, "touch", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            files.push(arg.clone());
        }
    }
    if files.is_empty() {
        return usage(ctx.stderr, "touch", "touch [-c] FILE...");
    }

    let mut status = 0;
    for file in files {
        if ctx.fs.exists(&file) {
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

pub(super) fn find(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut paths: Vec<String> = Vec::new();
    let mut name: Option<String> = None;
    let mut kind: Option<Kind> = None;
    let mut max_depth: Option<usize> = None;
    let mut min_depth = 0usize;
    let mut print0 = false;
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-name" => {
                index += 1;
                name = args.get(index).cloned();
            }
            "-type" => {
                index += 1;
                kind = match args.get(index).map(String::as_str) {
                    Some("f") => Some(Kind::File),
                    Some("d") => Some(Kind::Dir),
                    _ => None,
                };
            }
            "-maxdepth" => {
                index += 1;
                max_depth = args.get(index).and_then(|value| parse_usize(value));
            }
            "-mindepth" => {
                index += 1;
                min_depth = args
                    .get(index)
                    .and_then(|value| parse_usize(value))
                    .unwrap_or(0);
            }
            "-print" | "-print0" => print0 = arg == "-print0",
            _ if is_option(arg) => {
                error(ctx.stderr, "find", &format!("unknown predicate '{arg}'"));
                return 2;
            }
            _ => paths.push(arg.clone()),
        }
        index += 1;
    }

    if paths.is_empty() {
        paths.push(String::from("."));
    }

    let mut status = 0;
    for path in paths {
        let mut found = false;
        if let Err(err) = find_walk(
            ctx,
            &path,
            0,
            name.as_deref(),
            kind,
            max_depth,
            min_depth,
            print0,
            &mut found,
        ) {
            path_error(ctx.stderr, "find", &path, err);
            status = 1;
        }
    }
    status
}

#[allow(clippy::too_many_arguments)]
fn find_walk(
    ctx: &mut Ctx<'_>,
    display: &str,
    depth: usize,
    name: Option<&str>,
    kind: Option<Kind>,
    max_depth: Option<usize>,
    min_depth: usize,
    print0: bool,
    found: &mut bool,
) -> Result<(), FsError> {
    let meta = ctx.fs.meta(display)?;
    let name_ok = name
        .map(|pattern| glob_match(pattern, base_name(display)))
        .unwrap_or(true);
    let kind_ok = kind.map(|expected| meta.kind == expected).unwrap_or(true);
    if depth >= min_depth && name_ok && kind_ok {
        let terminator = if print0 { '\0' } else { '\n' };
        write_out(ctx.stdout, format_args!("{display}{terminator}"));
        *found = true;
    }

    if meta.kind == Kind::Dir && max_depth.map(|max| depth < max).unwrap_or(true) {
        let entries = ctx.fs.read_dir(display)?;
        for entry in entries {
            let child = fs_join(display, &entry.name);
            find_walk(
                ctx,
                &child,
                depth + 1,
                name,
                kind,
                max_depth,
                min_depth,
                print0,
                found,
            )?;
        }
    }
    Ok(())
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
        write_line(
            ctx.stdout,
            format_args!("  Size: {:<10} Type: {kind}", meta.size),
        );
        if meta.kind == Kind::Dir {
            match ctx.fs.read_dir(&absolute) {
                Ok(entries) => write_line(ctx.stdout, format_args!("  Entries: {}", entries.len())),
                Err(_) => {}
            }
        }
        write_line(
            ctx.stdout,
            format_args!("  Mode: 0755       Uid: 1000       Gid: 1000"),
        );
    }
    status
}

pub(super) fn tree(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut all = false;
    let mut max_depth = usize::MAX;
    let mut paths: Vec<String> = Vec::new();

    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-a" {
            all = true;
        } else if arg == "-L" {
            index += 1;
            max_depth = args
                .get(index)
                .and_then(|value| parse_usize(value))
                .unwrap_or(usize::MAX);
        } else if is_option(arg) {
            error(ctx.stderr, "tree", &format!("invalid option '{arg}'"));
            return 2;
        } else {
            paths.push(arg.clone());
        }
        index += 1;
    }
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
            write_line(ctx.stdout, format_args!("{prefix}{branch}{}/", entry.name));
            let next = if last {
                format!("{prefix}    ")
            } else {
                format!("{prefix}|   ")
            };
            let child = fs_join(path, &entry.name);
            tree_walk(ctx, &child, &next, depth + 1, max_depth, all, dirs, files)?;
        } else {
            *files += 1;
            write_line(ctx.stdout, format_args!("{prefix}{branch}{}", entry.name));
        }
    }
    Ok(())
}

pub(super) fn du(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut summary = false;
    let mut human = false;
    let mut all = false;
    let mut paths: Vec<String> = Vec::new();

    for arg in args {
        if is_option(arg) {
            for flag in arg[1..].chars() {
                match flag {
                    's' => summary = true,
                    'h' => human = true,
                    'a' => all = true,
                    other => {
                        error(ctx.stderr, "du", &format!("invalid option -- '{other}'"));
                        return 2;
                    }
                }
            }
        } else {
            paths.push(arg.clone());
        }
    }
    if paths.is_empty() {
        paths.push(String::from("."));
    }

    let mut status = 0;
    for path in paths {
        let mut total = 0usize;
        if let Err(err) = du_walk(ctx, &path, summary, all, human, &mut total, true) {
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
    root: bool,
) -> Result<usize, FsError> {
    let meta = ctx.fs.meta(path)?;
    let mut size = match meta.kind {
        Kind::File => meta.size,
        Kind::Dir => 0,
    };

    if meta.kind == Kind::Dir {
        let entries = ctx.fs.read_dir(path)?;
        for entry in entries {
            let child = fs_join(path, &entry.name);
            size += du_walk(ctx, &child, summary, all, human, total, false)?;
        }
    }

    *total += match meta.kind {
        Kind::File => meta.size,
        Kind::Dir => 0,
    };

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

pub(super) fn df(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let human = args.iter().any(|arg| arg.contains('h'));
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
    let percent = if memory.total == 0 {
        0
    } else {
        memory.used.saturating_mul(100) / memory.total
    };
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
