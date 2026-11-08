//! `tar` and `gzip`, built on the `feos-archive` and `feos-compress` libraries.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_archive::tar::{TarReader, TarWriter};
use feos_compress::deflate::Deflater;
use feos_compress::{gzip as gz, CompressError};

use super::opts::{bad_option, Opt, Opts};
use super::{error, path_error, read_path, usage, write_line, Ctx, MAX_WALK_DEPTH};
use crate::fs::{join as fs_join, FsError, Kind};
use crate::io::MAX_STREAM_BYTES;

/// Largest buffer either tool will allocate. A gzip header can claim any size,
/// so decompression grows toward this instead of trusting it.
const MAX_ARCHIVE_BYTES: usize = MAX_STREAM_BYTES;

fn zeroed(length: usize) -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(length).ok()?;
    buffer.resize(length, 0);
    Some(buffer)
}

fn gzip_bytes(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    // Fixed-Huffman DEFLATE can expand slightly, so leave headroom.
    let bound = data.len() + data.len() / 2 + 128;
    let mut output = zeroed(bound).ok_or("out of memory")?;
    let mut deflater = Box::new(Deflater::new());
    let written =
        gz::compress_with(&mut deflater, data, &mut output).map_err(|_| "compression failed")?;
    output.truncate(written);
    Ok(output)
}

fn gunzip_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    // The trailer's size is a hint from the file; verify by decompressing.
    let hint = match data.len().checked_sub(4) {
        Some(at) => {
            let tail = &data[at..];
            u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as usize
        }
        None => 0,
    };
    let mut capacity = hint.clamp(4096, MAX_ARCHIVE_BYTES);
    loop {
        let mut output = zeroed(capacity).ok_or_else(|| String::from("out of memory"))?;
        match gz::decompress(data, &mut output) {
            Ok(written) => {
                output.truncate(written);
                return Ok(output);
            }
            Err(CompressError::BufferTooSmall { .. }) if capacity < MAX_ARCHIVE_BYTES => {
                capacity = capacity.saturating_mul(2).min(MAX_ARCHIVE_BYTES);
            }
            Err(CompressError::BufferTooSmall { .. }) => {
                return Err(String::from("decompressed data is too large"));
            }
            Err(err) => return Err(format!("{err}")),
        }
    }
}

fn is_gzip(data: &[u8]) -> bool {
    data.starts_with(&[0x1f, 0x8b])
}

pub(super) fn gzip(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    gzip_command(args, ctx, "gzip", false)
}

pub(super) fn gunzip(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    gzip_command(args, ctx, "gunzip", true)
}

pub(super) fn zcat(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    let mut with_flag = alloc::vec![String::from("-c")];
    with_flag.extend_from_slice(args);
    gzip_command(&with_flag, ctx, "zcat", true)
}

fn gzip_command(args: &[String], ctx: &mut Ctx<'_>, cmd: &str, mut decompress: bool) -> i32 {
    let mut to_stdout = false;
    let mut keep = false;
    let mut force = false;
    let mut opts = Opts::new(args, "dcfkqvn123456789");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('d') => decompress = true,
            Opt::Flag('c') => to_stdout = true,
            Opt::Flag('k') => keep = true,
            Opt::Flag('f') => force = true,
            Opt::Flag('q' | 'v' | 'n' | '1'..='9') => {}
            other => return bad_option(ctx, cmd, other),
        }
    }
    let files = core::mem::take(&mut opts.operands);
    let convert = |data: &[u8]| -> Result<Vec<u8>, String> {
        if decompress {
            gunzip_bytes(data)
        } else {
            gzip_bytes(data).map_err(String::from)
        }
    };

    if files.is_empty() {
        let data = match ctx.stdin.try_read_to_end(MAX_STREAM_BYTES) {
            Ok(data) => data,
            Err(err) => {
                error(ctx.stderr, cmd, err.text());
                return 1;
            }
        };
        return match convert(&data) {
            Ok(output) => {
                ctx.stdout.write(&output);
                0
            }
            Err(message) => {
                error(ctx.stderr, cmd, &message);
                1
            }
        };
    }

    let mut status = 0;
    for file in &files {
        let target = if decompress {
            match file.strip_suffix(".gz").or(file.strip_suffix(".tgz")) {
                Some(stem) if file.ends_with(".tgz") => format!("{stem}.tar"),
                Some(stem) => stem.to_string(),
                None if to_stdout => String::new(),
                None => {
                    error(ctx.stderr, cmd, &format!("{file}: unknown suffix -- ignored"));
                    status = 1;
                    continue;
                }
            }
        } else {
            format!("{file}.gz")
        };
        if !to_stdout && !force && ctx.fs.exists(&target) {
            error(ctx.stderr, cmd, &format!("{target} already exists; not overwritten"));
            status = 1;
            continue;
        }
        let data = match read_path(ctx, file) {
            Ok(data) => data,
            Err(err) => {
                path_error(ctx.stderr, cmd, file, err);
                status = 1;
                continue;
            }
        };
        let output = match convert(&data) {
            Ok(output) => output,
            Err(message) => {
                error(ctx.stderr, cmd, &format!("{file}: {message}"));
                status = 1;
                continue;
            }
        };
        if to_stdout {
            ctx.stdout.write(&output);
            continue;
        }
        if let Err(err) = ctx.fs.write_file(&target, &output, false) {
            path_error(ctx.stderr, cmd, &target, err);
            status = 1;
            continue;
        }
        if !keep {
            if let Err(err) = ctx.fs.remove_file(file) {
                path_error(ctx.stderr, cmd, file, err);
                status = 1;
            }
        }
    }
    status
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TarMode {
    Create,
    List,
    Extract,
}

const TAR_USAGE: &str = "tar -c|-t|-x [-vz] [-C DIR] [-f FILE] [path...]";

pub(super) fn tar(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    // `tar czf out.tar dir` with no leading dash is the traditional form.
    let mut owned: Vec<String> = args.to_vec();
    if let Some(first) = owned.first_mut() {
        if !first.starts_with('-') && first.chars().all(|ch| "ctxvzfC".contains(ch)) {
            first.insert(0, '-');
        }
    }

    let mut mode: Option<TarMode> = None;
    let mut verbose = false;
    let mut compress = false;
    let mut archive: Option<String> = None;
    let mut directory: Option<String> = None;
    let mut opts = Opts::new(&owned, "ctxvzf:C:");
    while let Some(opt) = opts.next() {
        match opt {
            Opt::Flag('c') => mode = Some(TarMode::Create),
            Opt::Flag('t') => mode = Some(TarMode::List),
            Opt::Flag('x') => mode = Some(TarMode::Extract),
            Opt::Flag('v') => verbose = true,
            Opt::Flag('z') => compress = true,
            Opt::Value('f', value) => archive = Some(value.to_string()),
            Opt::Value('C', value) => directory = Some(value.to_string()),
            other => return bad_option(ctx, "tar", other),
        }
    }
    let Some(mode) = mode else {
        return usage(ctx.stderr, "tar", TAR_USAGE);
    };
    let paths = core::mem::take(&mut opts.operands);
    let archive = archive.filter(|name| name != "-");

    if mode == TarMode::Create {
        return tar_create(ctx, archive, directory, &paths, compress, verbose);
    }

    let source = archive.as_deref().unwrap_or("-");
    let data = match read_path(ctx, source) {
        Ok(data) => data,
        Err(err) => {
            path_error(ctx.stderr, "tar", source, err);
            return 2;
        }
    };
    let data = if is_gzip(&data) {
        match gunzip_bytes(&data) {
            Ok(data) => data,
            Err(message) => {
                error(ctx.stderr, "tar", &message);
                return 2;
            }
        }
    } else {
        data
    };
    tar_read(ctx, &data, mode, directory, &paths, verbose)
}

fn tar_create(
    ctx: &mut Ctx<'_>,
    archive: Option<String>,
    directory: Option<String>,
    paths: &[String],
    compress: bool,
    verbose: bool,
) -> i32 {
    if paths.is_empty() {
        error(ctx.stderr, "tar", "refusing to create an empty archive");
        return 2;
    }
    let base = directory.unwrap_or_default();
    let resolve = |name: &str| -> String {
        if base.is_empty() || name.starts_with('/') {
            name.to_string()
        } else {
            fs_join(&base, name)
        }
    };

    // Gather first, so the buffer can be sized exactly.
    let mut entries: Vec<(String, Kind)> = Vec::new();
    let mut total = 1024usize;
    for path in paths {
        let name = path.trim_end_matches('/');
        let name = if name.is_empty() { "/" } else { name };
        if let Err(err) = tar_collect(ctx, &resolve(name), name, &mut entries, &mut total, 0) {
            path_error(ctx.stderr, "tar", path, err);
            return 2;
        }
    }
    if total > MAX_ARCHIVE_BYTES {
        path_error(ctx.stderr, "tar", "archive", FsError::NoSpace);
        return 2;
    }
    let Some(mut buffer) = zeroed(total) else {
        path_error(ctx.stderr, "tar", "archive", FsError::NoSpace);
        return 2;
    };

    let length = {
        let mut writer = TarWriter::new(&mut buffer);
        for (name, kind) in &entries {
            if verbose {
                write_line(ctx.stderr, format_args!("{name}"));
            }
            // Stored names are relative, as tar conventionally strips the `/`.
            let stored = name.trim_start_matches('/');
            let result = match kind {
                Kind::Dir => {
                    let mut dir = stored.to_string();
                    dir.push('/');
                    writer.write_dir(&dir)
                }
                Kind::File => match ctx.fs.read_file(&resolve(name)) {
                    Ok(data) => writer.write_file(stored, &data),
                    Err(err) => {
                        path_error(ctx.stderr, "tar", name, err);
                        return 2;
                    }
                },
            };
            if let Err(err) = result {
                error(ctx.stderr, "tar", &format!("{name}: {err}"));
                return 2;
            }
        }
        match writer.finish() {
            Ok(length) => length,
            Err(err) => {
                error(ctx.stderr, "tar", &format!("{err}"));
                return 2;
            }
        }
    };
    buffer.truncate(length);

    if compress {
        buffer = match gzip_bytes(&buffer) {
            Ok(data) => data,
            Err(message) => {
                error(ctx.stderr, "tar", message);
                return 2;
            }
        };
    }
    match archive {
        Some(path) => {
            if let Err(err) = ctx.fs.write_file(&path, &buffer, false) {
                path_error(ctx.stderr, "tar", &path, err);
                return 2;
            }
        }
        None => ctx.stdout.write(&buffer),
    }
    0
}

fn tar_collect(
    ctx: &mut Ctx<'_>,
    actual: &str,
    name: &str,
    entries: &mut Vec<(String, Kind)>,
    total: &mut usize,
    depth: usize,
) -> Result<(), FsError> {
    if depth > MAX_WALK_DEPTH {
        return Err(FsError::TooDeep);
    }
    let meta = ctx.fs.meta(actual)?;
    entries.push((name.to_string(), meta.kind));
    // A header block, the data rounded up to blocks, and room for the long
    // name records the writer may need.
    *total = total
        .saturating_add(1024)
        .saturating_add(name.len())
        .saturating_add(meta.size.saturating_add(511) / 512 * 512);
    if meta.kind == Kind::Dir {
        for entry in ctx.fs.read_dir(actual)? {
            let child_actual = fs_join(actual, &entry.name);
            let child_name = fs_join(name, &entry.name);
            tar_collect(ctx, &child_actual, &child_name, entries, total, depth + 1)?;
        }
    }
    Ok(())
}

/// True when an archive member path would leave the extraction directory.
fn unsafe_member(name: &str) -> bool {
    feos_archive::safe_path(name).is_none()
}

fn tar_read(
    ctx: &mut Ctx<'_>,
    data: &[u8],
    mode: TarMode,
    directory: Option<String>,
    paths: &[String],
    verbose: bool,
) -> i32 {
    let base = directory.unwrap_or_default();
    let mut reader = TarReader::new(data);
    let mut status = 0;
    let mut name_buffer = alloc::vec![0u8; 8192];
    loop {
        let entry = match reader.next() {
            Ok(Some(entry)) => entry,
            Ok(None) => return status,
            Err(err) => {
                error(ctx.stderr, "tar", &format!("{err}"));
                return 2;
            }
        };
        let name = match entry.path(&mut name_buffer) {
            Ok(name) => name.to_string(),
            Err(err) => {
                error(ctx.stderr, "tar", &format!("{err}"));
                status = 2;
                continue;
            }
        };
        let trimmed = name.trim_start_matches('/').trim_end_matches('/');
        let selected = paths.is_empty()
            || paths.iter().any(|path| {
                let path = path.trim_end_matches('/');
                trimmed == path
                    || trimmed
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('/'))
            });
        if !selected {
            continue;
        }

        if mode == TarMode::List {
            write_line(ctx.stdout, format_args!("{name}"));
            continue;
        }
        if verbose {
            write_line(ctx.stdout, format_args!("{name}"));
        }
        if unsafe_member(trimmed) {
            error(ctx.stderr, "tar", &format!("{name}: refusing a path with '..'"));
            status = 2;
            continue;
        }
        let target = if base.is_empty() {
            trimmed.to_string()
        } else {
            fs_join(&base, trimmed)
        };
        let result = if entry.is_dir() {
            make_parents(ctx, &target)
        } else if entry.is_file() {
            let parent = crate::fs::parent(&ctx.fs.resolve(&target)).to_string();
            make_parents(ctx, &parent).and_then(|()| ctx.fs.write_file(&target, entry.data, false))
        } else if entry.is_symlink() {
            // Its target isn't checked: a link can point anywhere, and only what's
            // written through it is confined, by the path checks on later members.
            match entry.link() {
                Ok(link) => ctx.fs.symlink(link, &target),
                Err(_) => Err(FsError::InvalidPath),
            }
        } else {
            // Devices have no equivalent on this filesystem.
            Err(FsError::NotSupported)
        };
        if let Err(err) = result {
            path_error(ctx.stderr, "tar", &name, err);
            status = 2;
        }
    }
}

fn make_parents(ctx: &mut Ctx<'_>, path: &str) -> Result<(), FsError> {
    let absolute = ctx.fs.resolve(path);
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

/// `appinstall FILE [SHA256]`: installs a package into the application folder.
///
/// The digest comes from the command line or from `FILE.sha256`, never from the
/// package. There's no installing without one.
pub(super) fn appinstall(args: &[String], ctx: &mut Ctx<'_>) -> i32 {
    use feos_bundle::install;
    const USAGE: &str = "appinstall FILE [SHA256]";
    let (file, supplied) = match args {
        [file] => (file, None),
        [file, digest] => (file, Some(digest.clone())),
        _ => return usage(ctx.stderr, "appinstall", USAGE),
    };
    let data = match ctx.fs.read_file(file) {
        Ok(data) => data,
        Err(err) => {
            path_error(ctx.stderr, "appinstall", file, err);
            return 1;
        }
    };
    let digest = supplied.or_else(|| {
        let sidecar = ctx.fs.read_file(&format!("{file}{}", install::DIGEST_SUFFIX)).ok()?;
        let text = core::str::from_utf8(&sidecar).ok()?;
        install::parse_digest(text).map(ToString::to_string)
    });
    let Some(digest) = digest else {
        error(ctx.stderr, "appinstall", "no SHA-256 digest given, and no FILE.sha256");
        return 1;
    };
    let manifest = match install::verify_package(&data, &digest) {
        Ok(manifest) => manifest,
        Err(problem) => {
            error(ctx.stderr, "appinstall", &format!("{file}: {problem}"));
            return 1;
        }
    };

    let id = manifest.id.as_str();
    let target = format!("{}/{id}{}", install::APPS_DIR, install::PACKAGE_SUFFIX);
    let result = make_parents(ctx, install::APPS_DIR)
        // Digest first: a package is only ever seen with one, and a stale digest
        // just makes the scan skip the package until this finishes.
        .and_then(|()| {
            let digest_file = format!("{target}{}", install::DIGEST_SUFFIX);
            ctx.fs.write_file(&digest_file, digest.as_bytes(), false)
        })
        .and_then(|()| ctx.fs.write_file(&target, &data, false))
        .and_then(|()| ctx.fs.sync());
    if let Err(err) = result {
        path_error(ctx.stderr, "appinstall", &target, err);
        return 1;
    }
    write_line(ctx.stdout, format_args!("installed {id} at {target}"));
    0
}
