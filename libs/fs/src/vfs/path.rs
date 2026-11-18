//! Resolving the paths programs give, by the rules of section 3.3 of the ABI design.
//!
//! A program's path is relative to a directory it names (a `dirfd`, or its working
//! directory), and each of those is remembered as the canonical path it was opened by.
//! So resolution here joins strings and hands the result to the tree, whose walk keeps a
//! canonical path as it goes: a `..` is the parent of where the walk has got to, every
//! link before it already resolved, so it's physical, never the parent of a link that led
//! there. On top of the tree's walk this adds what POSIX says of a trailing slash (the
//! path must name a directory, after following a final link) and hides [`ORPHANS`].
//!
//! Kernel apps use the tree directly and see none of this.

use alloc::string::String;

use super::{FileKind, Resolved, VfsTree, ORPHANS};
use crate::FsError;

/// Whether `path` ends in a slash after a name: POSIX's "must be a directory". A path of
/// nothing but slashes is the root, which is one anyway.
pub fn has_trailing_slash(path: &str) -> bool {
    path.ends_with('/') && path.bytes().any(|byte| byte != b'/')
}

/// The last name in `path`, slashes after it ignored: `None` for the root.
pub fn last_component(path: &str) -> Option<&str> {
    path.rsplit('/').find(|part| !part.is_empty())
}

/// Whether any name in `path` is [`ORPHANS`], which no program may reach: its lookup is
/// `ENOENT`, wherever it is, so nothing can be made by that name for a program to see
/// into either.
pub fn is_hidden(path: &str) -> bool {
    path.split('/').any(|part| part == ORPHANS)
}

/// The longest name a program may give or be given, as POSIX's `NAME_MAX`: a directory
/// entry's record has room for no more (section 1 of the ABI design).
pub const NAME_MAX: usize = 255;

/// Whether any name in `path` is longer than [`NAME_MAX`]: `ENAMETOOLONG`, to look it up
/// or to make it.
pub fn has_long_name(path: &str) -> bool {
    path.split('/').any(|part| part.len() > NAME_MAX)
}

/// `given` relative to the directory `base` (a canonical path), or `given` itself if it's
/// absolute. On the heap, fallibly: either may be `PATH_MAX` long.
pub fn join(base: &str, given: &str) -> Result<String, FsError> {
    let mut out = String::new();
    if given.starts_with('/') {
        out.try_reserve_exact(given.len()).map_err(|_| FsError::OutOfMemory)?;
        out.push_str(given);
        return Ok(out);
    }
    out.try_reserve_exact(base.len() + 1 + given.len()).map_err(|_| FsError::OutOfMemory)?;
    out.push_str(base);
    if !base.ends_with('/') {
        out.push('/');
    }
    out.push_str(given);
    Ok(out)
}

/// Resolves an absolute `path` as a program's call does: following a final link if
/// `follow_last` or the path ends in a slash, in which case it must come to a directory
/// (`NotDirectory`), and with [`ORPHANS`] nowhere on the way (`NotFound`).
pub fn lookup(tree: &dyn VfsTree, path: &str, follow_last: bool) -> Result<Resolved, FsError> {
    if is_hidden(path) {
        return Err(FsError::NotFound);
    }
    let trailing = has_trailing_slash(path);
    let found = tree.resolve_path(path, follow_last || trailing)?;
    // A link on the way may have led into it.
    if is_hidden(&found.canonical) {
        return Err(FsError::NotFound);
    }
    if trailing && found.stat.kind != FileKind::Directory {
        return Err(FsError::NotDirectory);
    }
    Ok(found)
}
