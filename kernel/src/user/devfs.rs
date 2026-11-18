//! The kernel's side of `/dev` and `/proc` (section 3.3 of the ABI design): what
//! `/proc` shows of the calling program, and the descriptor links that lead nowhere a
//! path can go.
//!
//! The tree's `/proc` (`feos_fs::vfs::devfs`) asks [`KernelView`] about whoever is
//! calling, with the tree's lock held: its pid, its program and its descriptors. The
//! view takes the process's file table for the descriptors, which is safe because
//! nothing holds that table's lock while it calls the tree (see `files`).
//!
//! A descriptor's link names the path `FD_PATH` gives, so following one opens the file
//! anew by that path, as Linux does. A pipe's (`pipe:[n]`) leads nowhere, and neither
//! does a file's whose name has gone since; opening or looking at one of those through
//! its link reaches the descriptor itself instead ([`descriptor_behind`]), its open file
//! description shared by the new descriptor, as Linux's "magic" links do for pipes. So
//! does opening one that leads to `/dev/console`: the console's descriptor, not the
//! node, whose bits only root passes.
//!
//! A target longer than the tree takes for a link (a file opened by a very long path)
//! reads as [`devfs::TOO_LONG`], which leads nowhere either, so the node is still a link
//! and following it reaches the descriptor.

use alloc::string::String;

use feos_fs::vfs::devfs::{self, ProcessView};
use feos_fs::vfs::{path, VfsTree, MAX_SYMLINK_HOPS};
use feos_fs::FileKind;

use super::files::{Object, Ofd};
use super::process::{self, Process};
use crate::sched;

/// What `/proc` shows: the calling program.
pub struct KernelView;

/// The calling program, if the caller is one, and the slot it's calling from.
fn caller() -> Option<(usize, &'static Process)> {
    let slot = sched::caller_slot()?;
    // The slot's task is the caller, so its process stays for the call.
    process::current(slot).map(|process| (slot, process))
}

/// A copy of `text`, or nothing if there's no heap for it.
fn copy(text: &str) -> Option<String> {
    let mut out = String::new();
    out.try_reserve_exact(text.len()).ok()?;
    out.push_str(text);
    Some(out)
}

/// What the link for a descriptor of `slot`'s referring to `ofd` leads to.
fn target_of(slot: usize, ofd: &Ofd) -> Option<String> {
    let pipe = |raw: u32| alloc::format!("pipe:[{raw}]");
    match &ofd.object {
        Object::PipeRead(id) | Object::PipeWrite(id) => Some(pipe(id.raw())),
        Object::Stream(stream) => match sched::stream_pipe(slot, *stream) {
            Some(id) => Some(pipe(id.raw())),
            None => copy("/dev/console"),
        },
        Object::Console => copy("/dev/console"),
        Object::File { .. } | Object::Directory { .. } | Object::Link { .. } => copy(ofd.path()?),
    }
}

impl ProcessView for KernelView {
    fn pid(&self) -> Option<u32> {
        caller().map(|(_, process)| process.pid())
    }

    fn exe(&self) -> Option<String> {
        caller().and_then(|(_, process)| copy(process.exe()))
    }

    fn descriptor_count(&self) -> usize {
        caller().map_or(0, |(_, process)| process.files().table.open_count())
    }

    fn nth_descriptor(&self, index: usize) -> Option<u32> {
        let (_, process) = caller()?;
        let files = process.files();
        let fd = files.table.iter().nth(index).map(|(fd, _)| fd as u32);
        drop(files);
        fd
    }

    fn descriptor_target(&self, fd: u32) -> Option<String> {
        let (slot, process) = caller()?;
        // Read in place, not taken out: a reference taken here might be the last once
        // another thread's close came in between, and letting go of a file's description
        // calls the tree, whose lock is held.
        let files = process.files();
        target_of(slot, files.table.get(fd as usize)?)
    }
}

/// The view the tree's `/proc` asks.
pub static KERNEL_VIEW: KernelView = KernelView;

/// Gives the tree's `/proc` its view of the caller. Before any program runs.
pub fn init() {
    devfs::set_process_view(&KERNEL_VIEW);
}

/// The descriptor `joined` (an absolute path) comes to through a chain of links ending in
/// `/proc/<pid>/fd/<n>`, for the calling process `process`. Asked when following the last
/// link found nothing (a pipe's, or a file's that has lost its name since) or found the
/// console: either way the descriptor is what's meant.
pub fn descriptor_behind(process: &Process, tree: &dyn VfsTree, joined: &str) -> Option<usize> {
    let mut current = path::join("/", joined).ok()?;
    for _ in 0..=MAX_SYMLINK_HOPS {
        let found = path::lookup(tree, &current, false).ok()?;
        if found.stat.kind != FileKind::Symlink {
            return None;
        }
        if let Some(fd) = devfs::descriptor_link(&found.canonical, process.pid()) {
            return Some(fd as usize);
        }
        let target = tree.read_link(&found.canonical).ok()?;
        let parent = match found.canonical.rsplit_once('/') {
            Some(("", _)) | None => "/",
            Some((parent, _)) => parent,
        };
        current = path::join(parent, &target).ok()?;
    }
    None
}
