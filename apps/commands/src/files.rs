//! `files`: a graphical file explorer plus text-mode filesystem commands.
//!
//! `files` with no arguments opens a full-screen explorer. It is a foreground
//! app (`App::foreground`), so launching it from the desktop hands over the
//! framebuffer until the user closes the window or presses `Esc`.
//!
//! Text-mode forms are useful from the serial shell:
//!
//! ```text
//! files ls [path]
//! files cat <path>
//! files tree [path]
//! files stat <path>
//! ```

use core::fmt::Write;

use feos_app::{App, Context};
use feos_console::{print, println};
use feos_fs::{EntryKind, FileSystem, NodeId};
use feos_text::Text;

mod explorer;

/// Maximum absolute path length the explorer displays.
const PATH_MAX: usize = 256;

/// The `files` application.
pub struct Files;

impl App for Files {
    fn name(&self) -> &'static str {
        "files"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["explorer"]
    }

    fn usage(&self) -> &'static str {
        "files [ls|cat|tree|stat] [path]"
    }

    fn description(&self) -> &'static str {
        "browse the read-only filesystem with an XP-style explorer"
    }

    fn foreground(&self) -> bool {
        true
    }

    fn run(&self, context: &mut Context<'_>) {
        let Some(fs) = context.platform.filesystem else {
            println!("files: no filesystem is available on this board");
            return;
        };

        let args = context.args.trim();
        if args.is_empty() {
            explorer::run(fs, context);
        } else {
            run_command(fs, args);
        }
    }
}

fn run_command(fs: &dyn FileSystem, args: &str) {
    let mut parts = args.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("").trim();

    match command {
        "ls" | "list" => list(fs, if path.is_empty() { "/" } else { path }),
        "cat" | "read" => cat(fs, path),
        "tree" => tree(fs, if path.is_empty() { "/" } else { path }),
        "stat" | "info" => stat(fs, path),
        other => println!("files: unknown command '{other}' (try ls, cat, tree or stat)"),
    }
}

fn list(fs: &dyn FileSystem, path: &str) {
    let Some(node) = fs.resolve(path) else {
        println!("files: {path}: not found");
        return;
    };
    if fs.is_file(node) {
        print_entry(fs, node);
        return;
    }

    let count = fs.child_count(node);
    println!("{count} item(s) in {path}");
    for index in 0..count {
        let Some(child) = fs.child_at(node, index) else {
            continue;
        };
        print_entry(fs, child);
    }
}

fn print_entry(fs: &dyn FileSystem, node: NodeId) {
    let kind = fs.kind(node).map(EntryKind::short).unwrap_or('?');
    let name = fs.name(node).unwrap_or("?");
    if fs.is_dir(node) {
        println!("  {kind}  {:<10}  {name}/", format_size(fs.size(node)));
    } else {
        println!("  {kind}  {:<10}  {name}", format_size(fs.size(node)));
    }
}

fn cat(fs: &dyn FileSystem, path: &str) {
    if path.is_empty() {
        println!("files: usage: files cat <path>");
        return;
    }
    let Some(node) = fs.resolve(path) else {
        println!("files: {path}: not found");
        return;
    };
    if !fs.is_file(node) {
        println!("files: {path}: is a directory");
        return;
    }

    let mut buffer = [0u8; 8192];
    let read = fs.read(node, 0, &mut buffer);
    match core::str::from_utf8(&buffer[..read]) {
        Ok(text) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
        Err(_) => println!("files: {path}: binary file ({} bytes)", fs.size(node)),
    }
    if read < fs.size(node) {
        println!("files: {path}: output truncated to {read} bytes");
    }
}

fn tree(fs: &dyn FileSystem, path: &str) {
    let Some(node) = fs.resolve(path) else {
        println!("files: {path}: not found");
        return;
    };
    let mut buffer = [0u8; PATH_MAX];
    println!("{}", fs.path(node, &mut buffer).unwrap_or(path));
    tree_children(fs, node, 1);
}

fn tree_children(fs: &dyn FileSystem, node: NodeId, depth: usize) {
    if depth > 8 {
        return;
    }
    for index in 0..fs.child_count(node) {
        let Some(child) = fs.child_at(node, index) else {
            continue;
        };
        for _ in 0..depth {
            print!("  ");
        }
        if fs.is_dir(child) {
            println!("{}/", fs.name(child).unwrap_or("?"));
            tree_children(fs, child, depth + 1);
        } else {
            println!(
                "{}  ({})",
                fs.name(child).unwrap_or("?"),
                format_size(fs.size(child))
            );
        }
    }
}

fn stat(fs: &dyn FileSystem, path: &str) {
    if path.is_empty() {
        println!("files: usage: files stat <path>");
        return;
    }
    let Some(node) = fs.resolve(path) else {
        println!("files: {path}: not found");
        return;
    };
    let mut buffer = [0u8; PATH_MAX];
    println!("Path:     {}", fs.path(node, &mut buffer).unwrap_or(path));
    println!(
        "Kind:     {}",
        fs.kind(node).map(EntryKind::name).unwrap_or("?")
    );
    println!("Size:     {} bytes", fs.size(node));
    if fs.is_dir(node) {
        println!("Children: {}", fs.child_count(node));
    }
}

pub(crate) fn format_size(bytes: usize) -> Text<16> {
    let mut text = Text::<16>::new();
    if bytes < 1024 {
        let _ = write!(text, "{bytes} B");
    } else if bytes < 1024 * 1024 {
        let _ = write!(text, "{} KB", (bytes + 512) / 1024);
    } else {
        let _ = write!(text, "{} MB", (bytes + 512 * 1024) / (1024 * 1024));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_formatted_for_the_list_columns() {
        assert_eq!(format_size(0).as_str(), "0 B");
        assert_eq!(format_size(1023).as_str(), "1023 B");
        assert_eq!(format_size(1024).as_str(), "1 KB");
        assert_eq!(format_size(1536).as_str(), "2 KB");
        assert_eq!(format_size(2 * 1024 * 1024).as_str(), "2 MB");
    }
}
